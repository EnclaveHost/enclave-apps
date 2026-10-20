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
| `POST /add-file` | generic runtime/kernel bytes, streamed with the same 8 MiB multipart parts and CID calculation; disabled by default. Answers `{cid, bytes}`. Requires direct wallet authorization and positive `maxFileBytes`; storing bytes does not admit a runtime. |
| `POST /add-json` | an app-config JSON OBJECT (catalog rev-7 large configs), 1 MiB cap in lockstep with the runner's `CONFIG_MAX_BYTES` |
| `POST /add-image` | an app thumbnail/banner: raster by magic bytes, or SVG through the strict fail-closed validator (reject, never sanitize). Answers `{cid, svg}` |
| `GET /healthz` | `{"ok":true}` — the gateway liveness contract deploy tooling waits on |

The WASM, JSON and image routes accept direct EOA wallet signatures when
`walletAudience` is configured, or legacy relay HMAC tokens when `uploadKey` is
configured. The opt-in file route requires a direct wallet signature.
Both paths enforce per-wallet and global daily byte budgets, and echo CORS for the
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
    "maxFileBytes": 0,
    "maxImageBytes": 4194304,
    "maxJsonBytes": 1048576,
    "perAddrDailyBytes": 4294967296,
    "globalDailyBytes": 17179869184,
    "jsonPerIpHourly": 60
  }
}
```

The `upload` block is optional; without it the pin routes answer 503.

`allowOrigins` lists every browser origin that may call the upload routes
cross-origin: the platform dashboard, and any independent mirror of it (for
example `"allowOrigins": ["https://enclave.host",
"https://dash.example.org"]`). It can also be one string of origins separated
by commas or spaces, so a `$ALLOW_ORIGINS` secret can carry it.

How entries are treated:
- Each entry is one exact origin, normalized the way browsers send `Origin`.
- Wildcards, `null`, paths and plain `http` (except on loopback) are dropped
  with a log line, so no configuration ever produces `*`.
- An absent key keeps the historical default, `["https://enclave.host"]`.
- `[]` admits no browser origin.

The routes never allow credentials. Their authority is the per-request wallet
signature (or relay token), so listing a mirror lets a page *ask* for an
upload; it cannot act as anyone. The deployment-bound wallet message is
unchanged by any origin.
`walletAudience` is this adapter’s full 32-byte deployment ID on Base. Set it
to enable uploads without a relay or shared authorization secret.
`uploadKey` is an optional compatibility secret shared with the api-relay
(reference a deployment secret, never inline it). All other keys default to the values
shown. `allowUnsigned: true` opens the WASM, JSON and image routes with no token —
dev/e2e only, never on a public deployment. Generic files remain wallet-only.
Set `maxFileBytes` to an explicit positive limit (at most 2147483648) to enable
`/add-file`; the default zero returns 503. It shares the four upload slots and
per-wallet/global budgets. It does not bypass WASM validation on `/add-wasm`,
change runtime admission or promise indefinite retention.

`endpoint` and `bucket` are required; the app still starts without them and
says so in the UI. Omit `credentials` for a public bucket (requests go
unsigned). `refreshSecs: 0` disables the timer (manual refresh only).
`api_key` is a shared secret of your choosing; when set, it protects the
mutating routes: refresh, upload, delete (reads stay open, they only serve
content-addressed data). Present it as `X-Api-Key: <key>` or `?key=<key>`;
on the fleet the TLS proxy strips `Authorization`, so Bearer only works
locally. A public deployment without an `api_key` leaves the bucket
writable by anyone: set one.

## Direct wallet upload protocol

Set `upload.walletAudience` to the full adapter deployment ID, for example
`0x7ae476a3a1e4b0b144248075ff6656a0a10c3ae4cea8b6e4ad2b59dd8989ce33`.
The client personal-signs this exact UTF-8 message (LF separators, no trailing
newline); address, service and SHA-256 are lowercase. `bytes` and `expiry`
are unsigned decimal integers. The hash is 64 hex digits without `0x`.

```text
enclave-storage-upload:v1
chainId:8453
service:<full deployment ID>
method:POST
path:</add-json, /add-image, /add-wasm or /add-file>
address:<wallet address>
sha256:<SHA-256 of the upload body>
bytes:<Content-Length>
expiry:<Unix seconds, at most 900 seconds ahead>
```

Send `x-upload-address`, `x-upload-expiry`, `x-upload-sha256`, and
`x-upload-signature` (65-byte Ethereum signature as `0x` hex). The guest
verifies the signature before accepting a streaming body, then checks the
actual hash and length before committing it. Bad direct signatures never
fall back to HMAC or unsigned mode. The configured service ID prevents a
signature being reused at another adapter; route binding prevents using
JSON authorization for a WASM, image or generic file upload. Repeating the same request
within its validity window is possible and still consumes upload quotas.

This path supports EOA signatures. Smart accounts (a Safe, a passkey account)
use the explicit v2 scheme below. Platform sessions still use the legacy
validating routes. Enabling this feature alone does not retire the relay.

## Smart-account upload protocol (ERC-1271, v2)

A smart account has no private key to personal-sign the v1 message. Instead,
its own contract approves the upload through `isValidSignature`, read on the
chain by readers the adapter owner pins. Turn it on with
`upload.contractWallets` next to `walletAudience`:

```json
"contractWallets": {
  "chainId": 8453,
  "rpcs": ["https://base-rpc.one.example", "https://base-rpc.two.example", "https://base-rpc.three.example"],
  "quorum": 2,
  "confirmations": 2,
  "max_age_secs": 120,
  "timeout_secs": 5,
  "max_bytes": 65536
}
```

Absent, v2 requests are refused (401). The config is checked at start; an
invalid block is logged and leaves v2 off. The rules:

- `rpcs` names 2 to 7 distinct reader hosts. Each must be `https`; `http` is
  allowed only on loopback. No credentials, query or fragment.
- `quorum` is at least 2. `chainId` defaults to 8453.
- The rest default to the values shown.

The grammar and the check are the apps' shared `wallet.contract_wallets`
verification (`src/contract_wallet.rs` is a verbatim copy, pinned by a test).

The client sends the following headers:

- `x-upload-scheme: erc1271-v2`. The scheme is explicit and is never inferred
  from a signature's length.
- `x-upload-address`: the account.
- `x-upload-sha256`.
- `x-upload-nonce`: 32 lowercase hex digits, random.
- `x-upload-expiry`: at most 900 seconds ahead.
- `x-upload-signature`: the account's ERC-1271 bytes as `0x` hex, at most
  6144 bytes.

The signature is over the EIP-191 hash of this exact text:

```text
enclave-storage-upload:v2
scheme:erc1271
chainId:<contractWallets.chainId>
service:<full deployment ID>
method:POST
path:</add-json, /add-image, /add-wasm or /add-file>
account:<account address>
sha256:<SHA-256 of the upload body>
bytes:<Content-Length>
nonce:<nonce>
expiry:<Unix seconds>
```

That hash is what the adapter passes to `isValidSignature(bytes32,bytes)`. For
a Safe, it is the usual Safe message signature over that hash.

How the adapter checks, before it accepts any body byte:

1. **Chain read.** Each reader states its chain and head. The check is made at
   the quorum-th highest head minus `confirmations`. Every reader answers with
   the block number, hash and time, and the `isValidSignature` answer read at
   that block hash.
2. **Agreement.** The upload is approved only when exactly one answer reaches
   `quorum` and says valid, so one lying or failing reader can neither approve
   nor veto. A split refuses. A block older than `max_age_secs` refuses.
3. **Replay.** Each account and nonce pair is used once (409 on reuse) until its
   expiry.
4. **Budget.** Chain checks are limited to 30 a minute (429 beyond).

After the body arrives, its hash and length are checked as for v1. The
per-wallet budgets count the account. A v2 header next to an `x-upload-token`
is refused, and a refused v2 request never falls back to v1, HMAC or unsigned
mode.

An account must already be deployed. An undeployed address has no code and is
refused. An ERC-6492 (counterfactual) signature is refused with "deploy the
account first". Deploy the account through its normal path, then upload.

There is no relay, issuer or service signer. The only authority is the
account's contract, as read by the readers you pin.

### Gateway reads from a dashboard

A browser publisher verifies every upload by reading the exact bytes back from
this adapter's `/ipfs/<cid>` in ranges. It also reads committed startup
settings from the same place. These reads apply to v1 and v2 alike.

For an origin listed in `upload.allowOrigins` (exact match only, no fallback),
the gateway answers:

- `GET` and `HEAD` with `access-control-allow-origin`, which echoes the origin,
  and exposes `content-range`, `content-length` and `etag`;
- an `OPTIONS` preflight allowing `range` and `cache-control`.

Other origins get no CORS headers, and credentials are never allowed. The bytes
are public and content-addressed; this grants no authority.

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
