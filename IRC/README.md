# nanircd — an IRC server as an Enclave wasm service app

A zero-dependency IRC server written in Rust, compiled to a **wasm32-wasip2
command component**, and shaped to the Enclave hosting platform's *service app*
contract: the wasm-manager launches it with `wasmtime run` and a wasi:sockets
grant, it binds the TCP port the platform assigns, and IRC clients reach it
through the enclave's WebSocket bridge — all inside the attested sandbox (no
filesystem, no host env beyond `ENCLAVE_PORTS`, no threads, memory-capped).

```
IRC client ──tcp──> websocat ──wss──> https://<enclave>/x/<dep_id>/tcp/6667
                                        │ (supervisor WebSocket⇄TCP bridge)
                                        └──tcp──> 127.0.0.1:<actual> nanircd.wasm
```

## What it implements

RFC 1459/2812 core, single server (no S2S linking):

- **Registration**: `NICK`, `USER`, `PASS` (accepted, unused), CAP negotiation
  (`CAP LS/LIST/REQ/END` — empty capability set, so modern clients register
  cleanly), welcome numerics 001–005 with `ISUPPORT`, `LUSERS`, `MOTD`.
- **Channels**: `JOIN` (incl. `JOIN 0`, keys), `PART`, `TOPIC`, `NAMES`,
  `LIST`, `KICK`, `INVITE`, channel modes `+n +t +k +l +o +v` (defaults `+nt`,
  channel creator gets `@`), empty ban-list replies so clients' join-time
  `MODE #chan b` probes don't error.
- **Messaging**: `PRIVMSG` / `NOTICE` to channels and users (comma lists),
  `AWAY` with 301 replies.
- **Queries**: `WHO`, `WHOIS`, `ISON`, `USERHOST`, `VERSION`, `TIME`.
- **Liveness**: server-side `PING` after 90s idle, ping timeout, registration
  timeout, send-queue and flood caps, 512-byte line discipline, ascii
  casemapping.

Limits (compiled in): 512 clients, 32 channels/user, 24-char nicks, 50-char
channel names, 390-char topics. State is in-memory only — the platform gives
apps no disk, which suits IRC: when the deployment ends, the network vanishes.

## Design notes

`wasm32-wasip2` has no threads, so the server is one non-blocking event loop
(`src/main.rs`): accept, read/dispatch, timers, flush, reap, then a 25ms idle
sleep (2ms under load). Rust `std::net` maps directly to wasi:sockets on this
target — no async runtime, no dependencies, ~230 KB component.

The one platform rule (`wasm/apps/README.md` in the enclave repo): **read `ENCLAVE_PORTS` and bind
the actual port, never hardcode.** `resolve_port()` prefers our logical entry
`tcp:6667=<actual>`, falls back to the first tcp entry, and only defaults to
6667 when `ENCLAVE_PORTS` is absent (local development). It binds loopback only —
that is where the supervisor's bridge connects, and the manager's port audit
kills apps that bind anything unassigned.

## Build

```bash
rustup target add wasm32-wasip2        # or your distro's wasip2 std
cargo build --release --target wasm32-wasip2
# → target/wasm32-wasip2/release/nanircd.wasm  (component, layer 1)
```

## Run + test locally

Exactly what the platform runs (flags match `wasm_manager.py`), with a port
remap to prove the `ENCLAVE_PORTS` contract is honored:

```bash
wasmtime run -Scli -Sp3 -Stcp -Sudp -Sinherit-network -Sallow-ip-name-lookup \
  -W max-memory-size=134217728 \
  --env ENCLAVE_PORTS=tcp:6667=26667 \
  target/wasm32-wasip2/release/nanircd.wasm
```

(`-Sp3` mirrors the platform: the manager enables the WASIp3 API surface for
apps that want component-model async; nanircd is wasip2 and ignores it.)

Then, in another terminal:

```bash
python3 test/smoke.py 26667     # two-client scripted session, exits 0 on pass
# or interactively:
irssi -c 127.0.0.1 -p 26667
```

Without `ENCLAVE_PORTS` it listens on plain `127.0.0.1:6667`.

## Deploy on Enclave

The server declares one logical port, `tcp:6667`. Two routes:

**A. Baked-in catalog** (attested with the wasm-manager image): copy
`nanircd.wasm` into `wasm/apps/`, add to `catalog.json`:

```json
{ "id": "nanircd", "name": "nanircd (IRC server)", "file": "nanircd.wasm",
  "description": "Ephemeral IRC network in the enclave. Bridge to tcp:6667.",
  "mem_mb": 128 }
```

then rebuild/release the `enclave-wasm-manager` image and deploy by id.

**B. On-chain app store** (`EnclaveAppCatalog` on Base): pin `nanircd.wasm` to
IPFS, `publishVersion(slug, ..., cid, memMb=128, ports="tcp:6667")`, get the
version Approved by the catalog owner, then create an `EnclaveDeployments`
work item referencing `ipfs://<cid>` with `firewall.ports=["tcp:6667"]`, fund
it, and claim-hint it to an enclave.

For an open network the deployment should be `public: true` (an IRC network
wants strangers); leave it false for an owner-only network (bridge connections
then need `?token=<JWT>`).

## Connecting as a user

Declared TCP ports ride the enclave's single attested origin as a WebSocket at
`/x/<dep_id>/tcp/6667` (always the *logical* port — the supervisor resolves the
per-deployment actual). Bridge it to a local socket with
[websocat](https://github.com/vi/websocat), one bridge per IRC connection:

```bash
websocat -b tcp-l:127.0.0.1:6667 wss://<enclave>/x/<dep_id>/tcp/6667
```

then point any IRC client at it:

```bash
irssi  -c 127.0.0.1 -p 6667          # or weechat, hexchat, ...
/join #enclave
```

## Layout

```
Cargo.toml          no dependencies; small opt-level=s component
src/main.rs         ENCLAVE_PORTS resolution + the event loop
src/server.rs       all state and command handling (single-threaded)
src/message.rs      RFC line parsing, casemapping, name validation
test/smoke.py       two-client end-to-end smoke test
```
