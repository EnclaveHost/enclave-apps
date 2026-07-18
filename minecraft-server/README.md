# nanmc — a Minecraft server as an Enclave wasm service app

A zero-dependency Minecraft server written in Rust, compiled to a
**wasm32-wasip2 command component**, and shaped to the Enclave hosting platform's
*service app* contract: the wasm-manager launches it with `wasmtime run` and a
wasi:sockets grant, it binds the TCP port the platform assigns, and Minecraft
clients reach it through the enclave's WebSocket bridge — all inside the
attested sandbox (no filesystem, no host env beyond `ENCLAVE_PORTS`, no threads,
memory-capped).

It speaks **protocol 47**, i.e. any vanilla **1.8–1.8.9** client (still
selectable in the official launcher: Installations → New → version 1.8.9).
There is no JVM in the enclave and never will be, so this is not Mojang's
server — it is the protocol reimplemented from scratch (~1.5k lines, no
crates), which is also why it targets 1.8: the last protocol generation that
a from-scratch server can speak completely.

```
1.8.9 client ──tcp──> websocat ──wss──> https://<enclave>/x/<dep_id>/tcp/15565
                                          │ (supervisor WebSocket⇄TCP bridge)
                                          └──tcp──> 127.0.0.1:<actual> nanmc.wasm
```

## What it implements

- **Server-list ping**: MOTD, player count, protocol handshake (and a clean
  rejection for non-1.8 clients at login).
- **Login**: offline mode (the enclave can't reach Mojang session servers),
  deterministic per-name UUIDs, name validation, duplicate-name and
  server-full handling. No compression, no encryption — frames stay simple.
- **World**: procedural rolling grass hills (value noise, fixed seed),
  bedrock/stone/dirt/grass strata with tall-grass and flower decoration,
  generated on demand and streamed in rings around each player (view
  distance 5 → 121 chunks), with correct 1.8 chunk payloads (little-endian
  block states, nibble light arrays, biomes), chunk unloads behind you, and
  a serialized-chunk cache. Time is frozen at noon.
- **Creative multiplayer**: tab list, spawn/despawn of other players,
  position/look sync (absolute entity teleports — no drift), arm-swing
  relay, chat (vanilla `chat.type.text` translate component), join/leave
  broadcasts, block **place and break** synced to everyone with
  server-authoritative rollback (reach checks, placement validation,
  replaceable tall grass, meta from item damage — colored wool works).
- **Liveness**: keep-alives every 10s, 30s timeout, pre-login timeouts,
  input frame caps, output backpressure (chunks stream only as fast as the
  socket drains).

Limits (compiled in): 20 players, 64 sockets, view distance 5. **Nothing is
persisted** — the platform gives apps no disk, so the world regenerates from
the seed on every restart and player edits live exactly as long as the
deployment. Treat it as an ephemeral build-together canvas, not a world of
record. No survival mechanics: no mobs, physics, inventory persistence,
health, or crafting — everyone is in creative.

## Design notes

`wasm32-wasip2` has no threads, so the server is one non-blocking event loop
(`src/main.rs`): accept, read/dispatch, timers + chunk streaming, flush, reap,
then a 25ms idle sleep (2ms under load). Rust `std::net` maps directly to
wasi:sockets on this target — no async runtime, no dependencies, ~193 KB
component.

The one platform rule (`wasm/apps/README.md` in the enclave repo): **read `ENCLAVE_PORTS` and bind
the actual port, never hardcode.** `resolve_port()` prefers our logical entry
`tcp:15565=<actual>`, falls back to the first tcp entry, and only defaults to
15565 when `ENCLAVE_PORTS` is absent (local development). It binds loopback only —
that is where the supervisor's bridge connects, and the manager's port audit
kills apps that bind anything unassigned.

Why 15565 and not Minecraft's 25565: the platform caps logical port labels at
19999. The logical port is just a label — players' local websocat shim (below)
listens on 25565 anyway, so stock clients notice nothing.

## Build

```bash
rustup target add wasm32-wasip2        # or your distro's wasip2 std
cargo build --release --target wasm32-wasip2
# → target/wasm32-wasip2/release/nanmc.wasm  (component, layer 1)
```

## Run + test locally

Exactly what the platform runs (flags match `wasm_manager.py`), with a port
remap to prove the `ENCLAVE_PORTS` contract is honored:

```bash
wasmtime run -Scli -Sp3 -Stcp -Sudp -Sinherit-network -Sallow-ip-name-lookup \
  -W max-memory-size=268435456 \
  --env ENCLAVE_PORTS=tcp:15565=25999 \
  target/wasm32-wasip2/release/nanmc.wasm
```

Then, in another terminal:

```bash
python3 test/smoke.py 25999   # status, 2 players, chat, movement, blocks
python3 test/walk.py  25999   # chunk streaming/unloads on a 12-chunk hike
```

Both are strict headless protocol-47 clients; they exit 0 on pass. Or connect
a real client: launcher → 1.8.9 → Direct Connect → `127.0.0.1:25999`.
Without `ENCLAVE_PORTS` it listens on plain `127.0.0.1:15565`.

## Deploy on Enclave

The server declares one logical port, `tcp:15565`, and needs no GPU. Two
routes:

**A. Baked-in catalog** (attested with the wasm-manager image): copy
`nanmc.wasm` into `wasm/apps/`, add to `catalog.json`:

```json
{ "id": "nanmc", "name": "nanmc (Minecraft 1.8.9 server)", "file": "nanmc.wasm",
  "description": "Ephemeral creative Minecraft world in the enclave. Bridge to tcp:15565.",
  "mem_mb": 256 }
```

then rebuild/release the `enclave-wasm-manager` image and deploy by id.

**B. On-chain app store** (`EnclaveAppCatalog` on Base): pin `nanmc.wasm` to
IPFS via the site's Apps tab, then
`publishVersion("nanmc", ..., cid, [0, 0, 256, 10], ports="tcp:15565")`
(no VRAM/GPU, 256 MB RAM, 10 CPU GFLOPS), get the version Approved by the
catalog owner, then create an `EnclaveDeployments` work item referencing
`ipfs://<cid>` with `firewall.ports=["tcp:15565"]`, fund it, and claim-hint
it to an enclave.

For an open server the deployment should be `public: true` (anyone may open
the bridge); leave it false for an owner-only world (bridge connections then
need `?token=<JWT>`).

## Connecting as a player

Minecraft's protocol is raw TCP, not TLS, so it can't ride the SNI relay —
players use the enclave's WebSocket bridge with
[websocat](https://github.com/vi/websocat), one shim per player, then connect
to it as if it were a LAN server:

```bash
websocat -b tcp-l:127.0.0.1:25565 wss://<enclave>/x/<dep_id>/tcp/15565
```

Launcher → 1.8.9 → Multiplayer → Direct Connect → `127.0.0.1` (25565 is the
default port, so the bare address works).

## Layout

```
Cargo.toml          no dependencies; small opt-level=s component
src/main.rs         ENCLAVE_PORTS resolution + the event loop
src/proto.rs        frames, varints, positions, NBT-skip, slots, UUIDs
src/world.rs        noise terrain, edits, 1.8 chunk serialization + cache
src/server.rs       handshake/status/login/play, players, sync (single-threaded)
test/smoke.py       strict two-client end-to-end protocol test
test/walk.py        chunk streaming / retarget / keep-alive soak test
```
