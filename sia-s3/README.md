# sia-s3

An S3 endpoint backed by [Sia](https://sia.tech), served from an attested enclave. Objects are encrypted and
erasure-coded **inside the enclave** by the Sia SDK (10 data + 20 parity shards, any 10 of 30 hosts recover an
object) and stored on independent Sia hosts through an indexer, by default the platform's own
[`https://sia.enclave.host`](indexd/README.md). Existing S3 clients work unchanged: point them at this app.

The key index lives in the objects' own sealed metadata, so the app keeps **no local state**. A restart rebuilds
the index from the indexer's object feed.

## Config

`ENCLAVE_CONFIG` (or `SIAS3_CONFIG` locally), JSON. `$NAME` string values resolve from the environment, which
is how deployment secrets arrive:

```json
{
  "indexer": "https://sia.enclave.host",
  "appKey": "$SIA_APP_KEY",
  "region": "auto",
  "buckets": ["ipfs"],
  "credentials": [{ "accessKeyId": "$S3_ACCESS_KEY_ID", "secretAccessKey": "$S3_SECRET_ACCESS_KEY" }]
}
```

- `appKey`: the 32-byte (hex) seed of an app key registered on the indexer (`POST /api/apps/register` on
  indexd's admin API, with a connect key whose quota covers the data). It encrypts everything; losing it loses
  the data, and leaking it exposes it.
- `buckets`: fixed by config (CreateBucket on an existing one is a no-op).
- `credentials`: every request must be SigV4-signed with one of these.
- Optional: `maxBodyBytes` (64 MiB; single PUT / part cap), `compactMaxObjectBytes` (8 MiB),
  `compactEverySecs` (1800).

## S3 surface

PUT/GET/HEAD/DELETE object, ranges (`bytes=a-b`, `a-`, `-n`), CopyObject (`x-amz-copy-source`),
ListObjectsV2 and v1 (prefix, delimiter, max-keys, continuation-token, start-after), DeleteObjects,
multipart (create, upload part, complete, abort, list uploads), ListBuckets, HeadBucket, GetBucketLocation.
Request bodies: Content-Length or `Transfer-Encoding: chunked` (wasi:http clients send even an empty DELETE body
chunked). Auth is SigV4 header auth with a hex `x-amz-content-sha256` (verified against the body) or `UNSIGNED-PAYLOAD`.
Conditional PUT: `If-None-Match: *` (create only) and `If-Match: <etag>` answer 412 when they do not hold, checked again
when the new object is committed. A single PUT above `maxBodyBytes` (up to 16 GiB) is not buffered: it streams to Sia
as it arrives, hashed on the way (a short body or a payload-hash mismatch stores nothing).
Not supported: presigned URLs, aws-chunked streaming uploads, ACLs, versioning, tagging. `GET /healthz` is
unauthenticated and reports counts only.

## File explorer

An unsigned `GET /` serves a page with the status, client setup, and a file explorer that opens with the S3
keys. The page signs every request itself (SigV4 in WebCrypto) and sends only signatures; nothing else in the
app changes. It has a folder tree, list and grid views, sorting and filtering, multi-select, a right-click menu,
keyboard control, drag and drop (files and folders in from the desktop, items onto folders to move them),
cut/copy/paste, rename, move or copy across folders and buckets, recursive delete, downloads, and previews
(images, video, audio, PDF, text; keys without an extension are sniffed). Notes:

- Rename, move and copy are CopyObject (plus DELETE for a move), so they rewrite metadata and never move data.
- Uploads above 48 MiB go multipart in 40 MiB parts. When the gateway in front of the app gives up on a slow
  upload (502/504) the page keeps checking until the key appears with the file's MD5 ETag (multipart: until
  Complete succeeds), for up to 20 minutes.
- A new folder exists only in the tab until a file is put in it: an empty marker object would cost a slab.
- Previews download whole objects (text: the first 256 KiB) and are capped by type; downloads are saved as
  `application/octet-stream`, and HTML is never rendered.

## How keys map to Sia objects

- PUT uploads the body as one Sia object whose metadata names the key, size, ETag (MD5) and mtime.
- CopyObject adds an entry to the source object's metadata, so no data moves. DELETE removes an entry, and an
  object left serving nothing is deleted.
- Multipart parts are objects tagged with their upload. Complete writes the key on part 1 and lists the
  other parts in order.
- The indexer refuses objects without data, so there are no standalone manifest objects.

**Small objects** cost a full slab when uploaded alone (4 MiB sectors on 30 hosts, about 120 MiB of upload
traffic and paid storage), and a lone small PUT takes on the order of 45 s. The **compactor** repacks
single-object keys under `compactMaxObjectBytes` into shared slabs after five minutes, then prunes the old
slabs. Reads are fast (a 1 MB range in about 0.6 s from 30 hosts).

## Bulk import

```bash
sia-s3 import <bucket> manifest.json   # manifest: [{"key","size","url"}]
```

The import fetches each URL in 32 MiB ranges, streams it into packed uploads (512 MiB batches), and pins with
the same metadata the server writes. Re-running skips keys already present with the same size. A running
server sees imported keys after its next index load (restart it).

## Build and run

```bash
cargo build --release       # .cargo/config.toml targets wasm32-wasip2 with --cfg tokio_unstable
wasmtime run -S inherit-network -S allow-ip-name-lookup \
  --env 'ENCLAVE_PORTS=http:8000=18490' --env SIA_APP_KEY --env 'ENCLAVE_CONFIG={...}' \
  target/wasm32-wasip2/release/sia-s3.wasm
S3CHECK_ENDPOINT=http://127.0.0.1:18490 S3CHECK_AK=... S3CHECK_SK=... python3 -I scripts/s3check.py test
```

Deploy on enclave.host CPU-only with port `http:8000`, egress enabled (the SDK dials the indexer and Sia hosts
on their announced ports, through the deployment's SOCKS front), and the secrets above.

## Vendored SDK

`vendor/sia-sdk-rs` is SiaFoundation/sia-sdk-rs at the commit in `UPSTREAM`, plus the wasip2 patch: browser-only
code is gated to `wasm32-unknown-unknown`, wasip2 uses the native SiaMux transport over wasi:sockets,
`reqwest_wasi` replaces reqwest's browser backend, every dial goes through `ENCLAVE_EGRESS` when set, and
blocking work runs inline (no threads on wasip2). MIT licensed, as upstream.

## History

`spike/` is the 2026-10-13 wasip2 feasibility test (SDK on wasip2, indexer over TLS, SiaMux to 20/20 hosts, the
SOCKS egress path). Its results are in this file's history (commit 13dce6b). `sia-sdk-rs-wasip2.patch` is that
patch as one file. `vendor/` carries it applied.
