# sia-s3 (spike)

Goal: an enclave app that serves an S3 endpoint backed by Sia (via the Sia
Storage indexer), so s3-ipfs-adapter and other apps can move off R2 by changing
only their endpoint.

Step 1, the feasibility spike, lives in `spike/`. Its question: does the Sia
Rust SDK (`sia_storage`) run as a wasm32-wasip2 component?

## SDK patch

`sia-sdk-rs-wasip2.patch` applies to SiaFoundation/sia-sdk-rs at fbee7d9. The
SDK treats every wasm32 target as a browser (WebTransport, fetch); the patch
changes that as follows:

- **Browser-only code** is gated to `wasm32-unknown-unknown`. wasip2 takes the
  native SiaMux-over-TCP path instead, on tokio + wasi:sockets.
- **reqwest_wasi** is a new crate that provides the subset of the reqwest API
  the SDK uses. It speaks HTTP/1.1 over tokio + rustls(ring) and is aliased in
  as `reqwest` on `target_os = "wasi"`.
- **Egress:** `reqwest_wasi::net::connect` is the single dialer. When
  `ENCLAVE_EGRESS` is set it goes through that SOCKS5 front, passing host
  names unresolved; otherwise it connects directly, resolving names inline.
- **No threads on wasip2:** `maybe_spawn_blocking` runs inline on all wasm.
  The memory budget is fixed at 256 MiB, overridable with
  `SIA_MEMORY_BUDGET`.

Build with `RUSTFLAGS="--cfg tokio_unstable"`, because tokio gates `net` on
wasm.

```
git clone https://github.com/SiaFoundation/sia-sdk-rs ../../sia-sdk-rs   # sibling of this worktree's parent
(cd ../../sia-sdk-rs && git checkout fbee7d9 && git am <this dir>/sia-sdk-rs-wasip2.patch)
cd spike && RUSTFLAGS="--cfg tokio_unstable" cargo build --release --target wasm32-wasip2
```

## Results (2026-10-13, wasmtime 49.0.1)

| check | result |
|---|---|
| SDK builds for wasm32-wasip2 | PASS (355 KB component, wasi 0.2 imports only) |
| HTTPS to the hosted indexer (sia.storage) via reqwest_wasi | PASS, 427 ms, approval URL issued |
| SiaMux handshake + host key check + RHP4 settings, 20 real hosts | PASS 20/20, 0.2-2 s each |
| Same through a SOCKS5 user/pass front (ENCLAVE_EGRESS shape) | PASS 6/6; names passed unresolved; wrong token fails closed |
| upload / pin / ranged download / object_events / delete | BLOCKED: waiting for the indexer approval (sign-in to a sia.storage account) |
| Through the real platform egress (a deployed test app) | NOT RUN; egress code blocks only port 25, Sia hosts use 9984 |

```
wasmtime run -S inherit-network -S allow-ip-name-lookup --dir KEYS::/keys \
  target/wasm32-wasip2/release/sia-spike.wasm connect|test|settings /keys/<file>
```
