# Experimental Wasm desktop controller

`src/bin/shield-desktop.rs` is a separate `wasm32-wasip2` component. The legacy
controller and deployed 0.6.11 image remain separate. This component is not yet
admitted or deployed by the NucBox runtime.

The controller requires `KVMBOX_OWNER_PASSWORD` through the verified secret
release path. It refuses a missing, empty, non-UTF-8 or over-1024-byte password,
and refuses any presence of the legacy `KVMBOX_ACCESS_SHA256`. It hashes the
password and removes the guest environment entry. This does not erase the
parent Wasmtime process's environment; the measured runner must isolate it.
An environment variable by itself proves no attestation or authorization.

Every route requires exactly one Basic `owner:<password>` credential. Accepted
requests go only to `127.0.0.1:19080`; the controller listens on
`127.0.0.1:8000`. Owner credentials and spoofable identity headers are removed.
`X-Compute-Authorization` remains available for the companion's separate compute
token check. The `/api` routes control the companion and `/desktop/` carries
desktop HTTP, noVNC WebSocket and the Moonlight tunnel protocol.

The single-thread loop limits connections to 32, queues to 64 KiB per direction,
headers to 16 KiB and request bodies to 8 MiB. Header and idle deadlines are five
and 300 seconds. A backend connect is bounded to 250 ms, including when its
accept queue is saturated. Several pending connects can still delay the loop;
this is a bounded synchronous connect, not a fully asynchronous connector.
Streams and half-closes otherwise progress independently.

Thirty native tests pass. Eight checks of the actual component under Wasmtime
49.0.3 pass in a private network namespace: startup refusals, authorization
before backend contact, fixed routing, a 2 MiB streamed body, credential
stripping, both upgrade protocols, half-close, six concurrent tunnels alongside
a slow HTTP response, action/origin/framing refusals, header expiry and saturated
backend connection handling. These use a fixture companion; they do not prove
the desktop, GPU service, sealed release or real Moonlight integration.

```sh
cargo test --locked --offline --bin shield-desktop
cargo build --locked --offline --release --target wasm32-wasip2 --bin shield-desktop
KVMBOX_TEST_PARENT_NETNS="$(readlink /proc/self/ns/net)" \
  unshare --user --map-root-user --net python3 tests/check_shield_desktop_wasi.py \
  target/wasm32-wasip2/release/shield-desktop.wasm /path/to/work/controller-checks
```

The native companion still needs its measured process lifecycle, authenticated
model mounts, masked worker reservation/transport and profile admission. Its
TCP gateway cannot authenticate a peer UID using Unix-socket credentials:
the domain's network policy must restrict the loopback gateway to the Wasm
runner's UID, and the worker channel to the trusted masking service's UID.
Neither the desktop nor a client-supplied header may bypass that boundary.
The owner connector also needs to route Moonlight through the controller's
`/desktop/api/moonlight/tunnel` endpoint for this new profile.

Claude supplied the initial controller, dashboard and native tests. Integration
added the bounded connect and actual Wasip2 checks. The new component stays
experimental until these remaining runtime and hardware checks pass.
