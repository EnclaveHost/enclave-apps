# network-test — "who am I on the network?"

A tiny Enclave (enclave.host) service app that demonstrates **dedicated-IP
egress**: every deployment gets its own IPv6, its declared ports are served on
it (inbound), and — with the phase-2 toolchain — **all of its outbound leaves
from that same address, with zero code changes**. This app proves each piece
from the inside and prints the evidence as one plain-text page.

It is deliberately **zero-dependency Rust** (`std` only): the transparent-egress
claim is about *unmodified* code, so the outbound probe is a bare
`std::net::TcpStream::connect`.

## What the page shows

```
[1] identity (SOCKS BND.ADDR, platform-derived):   2a01:4f9:c013:9b52:xxxx:...
[2] explicit egress fetch (icanhazip.com):         internet sees 2a01:...same
[3] transparent fetch (unmodified std::net):       internet sees 2a01:...same
    -> matches [1]: outbound is transparently source-tagged (phase 2 live)
[4] loopback dial 127.0.0.1:8080: DENIED — raw network closed ✓
```

- **[1]** dials the `ENCLAVE_EGRESS` front and speaks SOCKS5 (RFC 1928/1929) by
  hand; the CONNECT reply's `BND.ADDR` is this deployment's dedicated IPv6 as
  derived *by the enclave from the authenticated credential* — the platform
  telling you who you are. (Under the phase-2 lockdown this explicit dial
  works because the shim passes a dial to the front itself through.)
- **[2]** fetches a public ip-echo through that phase-1 tunnel: the internet's
  view of the source address.
- **[3]** does the same fetch with plain `std::net` — no proxy, no SOCKS, the
  code every app already has. Under the phase-2 toolchain it reports the same
  address as [1]/[2]: outbound was source-tagged without the app knowing.
- **[4]** dials the enclave's loopback (the supervisor port): denied, because
  with `-S egress` the guest has **no raw network at all** — that's what makes
  the identity un-bypassable (and closes loopback SSRF). `[4b]` shows a known
  quirk: a *non-blocking* `connect_timeout` may report a phantom `Ok` for a
  denied dial — the socket fails on its first read/write; no bytes ever flow.

The page is served on **both** declared ports, which is the other half of the
story: `http:8000` is the ordinary `/x/<id>` HTTP path, and `tcp:7777` is
reachable at `[<your dedicated IPv6>]:7777` via the tcp6-relay — the **same
address** your outbound leaves from. One identity, both directions.

Degraded modes (the app reports them honestly): egress env absent → `[1][2]`
skipped, `[3]` shows the enclave's shared source, `[4]` connects (raw
network); phase-1 toolchain → `[1][2]` work but `[3]` differs and `[4]`
connects.

## Build

```bash
rustup target add wasm32-wasip2      # once (or a distro Rust with wasip2 std)
cargo build --release --target wasm32-wasip2
# artifact: target/wasm32-wasip2/release/network-test.wasm (~170 KB)
```

## Publish + deploy (on the platform)

> **This is a run-mode (service) app — the open ports are MANDATORY.**
> network-test is a command component that binds raw sockets, not a
> `wasi:http` serve component. It only runs when the deployment declares
> open ports, which makes the enclave launch it with `wasmtime run`
> (+ wasi:sockets, + `-S egress`). **Deploy or publish it without the ports
> and the enclave falls back to `wasmtime serve`, which fails at instantiation
> with `no exported instance named wasi:http/incoming-handler` — the app never
> starts.** The ports are fixed on-chain at create time and can't be edited
> after, so a portless deployment can't be repaired — deploy a new one.

Specs:

| axis | value |
|---|---|
| GPU vram / compute | 0 / 0 (CPU app) |
| memory | 128 MB |
| node compute | 10 GFLOPS |
| storage_mb | 0 (no `/data` needed) |
| open ports | **`http:8000,tcp:7777`** (required) |

**Recommended — publish to the catalog with the ports baked in, then deploy by
slug** (every future deploy then carries the ports automatically, and it shows
up in the Apps tab):

```bash
enclave publish target/wasm32-wasip2/release/network-test.wasm \
  --slug network-test --name "Network test" --version 0.1.0 \
  --mem 128 --cpu-gflops 10 --ports "http:8000,tcp:7777"

enclave deploy network-test:0.1.0 --fund 1        # public by default
```

**Or deploy the raw CID directly — but then you MUST pass `--ports` yourself**
(a raw-CID deploy has no catalog version to supply them):

```bash
enclave deploy ipfs://<cid> --ports "http:8000,tcp:7777" --fund 1
```

Either way, once it's claimed and running:

```bash
curl https://<deployment-id>.app.enclave.host/       # via the HTTP path
curl "http://[<network.address>]:7777/"              # via the dedicated IPv6
```

Optional: a deployment `configCid` whose content is a bare `host:port` string
overrides the ip-echo target (default `icanhazip.com:80`; the echo must speak
plain HTTP and answer with the caller's address).

## Run locally (no platform needed)

`dev-run.mjs` stands up the REAL enclave-side front (`egress.js`) and the
REAL source-binding relay (`relay/egress-relay.js`), then launches the
component under a phase-2 patched wasmtime exactly like the wasm-manager does
(`-S egress`, credential in the host-side `ENCLAVE_EGRESS_CRED`, **no**
`-Sinherit-network`), and prints the page fetched from both ports:

```bash
cargo build --release --target wasm32-wasip2
ENCLAVE_REPO=~/Projects/enclave \
ENCLAVE_EGRESS_WASMTIME=/path/to/patched/wasmtime \
node dev-run.mjs
```

Local caveat: without a routed /64 on your box the derived identity is a
placeholder and the relay dials from your own address (`EGRESS_ALLOW_V4=1`,
v4-only echo), so `[1]` won't equal `[2]/[3]` — on a real enclave they match.
What the local run does prove: transparent mediation (`[3]` succeeds with no
raw network), the phase-1 front pass-through (`[1][2]` work under lockdown),
and the lockdown itself (`[4]` denied).
