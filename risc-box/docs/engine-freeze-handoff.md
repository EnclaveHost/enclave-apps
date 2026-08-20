# Handoff: finish the stable-30fps Moonlight chain by fixing the engine freeze

Paste everything below to the next agent. It is self-contained, but read
`moonlight-30fps-handoff.md` (the working chain) and `tunnel-stall-handoff.md`
(the defect, with the full evidence ladder) before touching anything — every
claim in both is measured, and three earlier theories died to sharper probes.

---

You are picking up the RISC Box → Moonlight streaming work in
`~/Projects/enclave-apps/risc-box` (public repo, source-available license;
platform repo `~/Projects/enclave/`; the operator's wasmtime tree is
`~/Projects/wasmtime`). Standing repo rules: commit and push to `main` after
any change, trailer `Co-Authored-By: Claude <noreply@anthropic.com>`; no
deploy scripts — the operator ships releases; never launch the moonlight GUI
on the operator's `:0`.

## Where it stands (2026-08-20, all measured)

The chain WORKS: FreeDoom on the emulated 1024x768 Alpine desktop, encoded to
H.264 **inside the enclave** (minih264 on the SET worker, 1-3 ms/frame on
metal0, 23 ms on kryptos), streamed as one SSE, repacketized by `gs-bridge
--frames h264` into GameStream RTP with no re-encode, decoded by a real
moonlight-common-c client while scripted gameplay input flows back. Under
continuous input: **min 38 / p5 39 / median 40 fps per second, 0 of 86
seconds below 30**; the guest itself renders 33-52 fps during driven play;
app-side production holds a flat 40 through 250 Hz mouse motion. Seven
pipeline defects were found and fixed to get there (bounded keyframes, an
absolute encode schedule, pipeline depth 2, coalesced pointer state, the
/hid-stream body-buffering blackhole, the streamPacketIndex u16 wrap, stale
session eviction) — the list with numbers is in `moonlight-30fps-handoff.md`,
and the git log from `risc-box 0.6.40` through `0.6.43` tells it as a story.

**One defect remains and it is not in this repo.** Over ~30-minute sessions
the tenant's ENTIRE socket set intermittently freezes 0.5-14 s (clusters,
worse minutes at a time), felt by the player whenever they do anything that
generates requests. The evidence ladder in `tunnel-stall-handoff.md`
exonerates, in order: the client's network, the raw path (ICMP to the serving
host flat through freezes), the relay/tunnel (reproduces on kryptos's direct
endpoint), the ingress shim and the supervisor's event loop (both probe flat
DURING tenant stalls), the box and other tenants (a second tenant answered
250/250 clean interleaved with 16 stalls on ours), and the app itself
(per-turn telemetry: worst turn 11-13 ms while a warm probe waited 14 s — the
loop polled through the whole freeze and was told its sockets had nothing).

**The differential that names the suspect**: the identical app as a plain
wasip2 build survived the same load 243/243 probes (worst 317 ms) where the
SET (shared-everything-threads) build stalls to 14.2 s; doubling the load
reproduces a milder 2.1 s form on plain. So: the fleet engine's wasip2 socket
readiness path is implicated at base, and the SET patch drops the trigger
threshold and raises the ceiling ~10x. Mechanism zone, specifically:
`crates/wasi/src/sockets/tcp.rs` (~620-690) `try_read`/`poll_read_ready`
tokio-readiness caching, entered from a sync guest via `in_tokio`'s per-call
`block_on` (`crates/wasi/src/runtime.rs`) — the "sync mode" whose own comment
admits pollable-looping guests can starve the background work that makes
sockets ready (one case is band-aided with a `yield_now`). The SET build runs
TWO such guest threads doing concurrent block_ons; every interleaving that
loses a readiness edge becomes an order of magnitude likelier.

## Your mission, in order

1. **Prove the mechanism in one shot** (needs the operator — only they can
   reach inside the CVM). Trigger a freeze (a minute of repro, below), then
   `ss -tin` on the app's loopback port inside the CVM: non-zero Recv-Q while
   the app's `turnMaxMs` stays ~10 ms is the whole case — kernel had the
   bytes, the engine's readiness layer did not lift them. Then
   `strace -e epoll_wait,epoll_ctl -p <wasmtime pid>` across a freeze, or
   tokio-console on a debug engine build.
2. **Fix it in the engine** (with the operator: fleet wasmtime = the pinned
   wasmtime-49-dev commit + patches built by
   `~/Projects/enclave/wasm/Dockerfile.wasmtime`;
   `wasmtime-set-threads.patch` is the SET layer). Candidate directions, in
   rising order of invasiveness: make sync-mode host calls drive the runtime's
   background work deterministically (the `yield_now` hack, done properly);
   audit what two concurrent guest-thread `block_on`s do to readiness-edge
   consumption; move socket readiness off cached edges to level-checks on
   entry. A local repro loop beats fleet iterations: the plain-vs-SET pair
   under `wasmtime run -S inherit-network` + the load below should reproduce
   without the fleet (unverified — confirming that locally is itself useful).
3. **Re-certify** once the engine is fixed: the 31-minute soak with
   continuous gameplay input, expecting what the clean stretches already
   show. The bar: ≥30 fps in ≥99% of per-second buckets over ≥25 minutes
   ("soak ≥25 min or the floor lies"), zero corrupt frames, input verified by
   decoded stream content (the harness picks up items and loses health).
4. Optional follow-ups, in value order: mouse-look (xdoom is keyboard-only —
   `usemouse=0` in `guest/fbdoom/i_video_x11raw.c`; needs a rebuild with
   pointer grab+warp via `build-xdoom.sh`, baked into `alpine/rootfs.ext2.gz`
   in R2, plus ideally a relative-motion path: the virtio-input device is
   absolute-only); binary framing for /video (base64-over-SSE taxes the wire
   33%); wasm SIMD128 for minih264 (kryptos's 23 ms encode is the ceiling
   there); the metal0 relay path's fixed 389 ms/request tax (separate, minor).

## The standing reproducer (no CVM access needed)

- Deployment `0x157cf55b533e8e565a4fd8e9fa78db822d195767997c01bfa2732aa5d9b3aecf`
  on kryptos, private, currently running catalog `risc-box-doom:0.7.17`
  (SET build, app 0.6.43 with turn telemetry). `0.7.18-plain` is the same
  app as a plain wasip2 build; one `build_upgrade` + `claim_hint
  {enclave:"kryptos"}` flips arms (in-place: endpoint and funds survive,
  ~4 min reboot; wait for `/status` `.fps > 3` = desktop up).
- Load: hold `curl -sN ".../x/<id>/video?codec=h264&kbps=3000" > /dev/null`
  open, and on a SEPARATE warm connection request `/status` ~7/s, logging
  per-request latency. SET: stalls >500 ms (some >10 s) within a minute.
  Plain: needs roughly double the load, tops out ~2 s.
- Read `/status` during load: `turnMaxMs`/`turnMax` (worst main-loop turn +
  per-phase split since last heartbeat — poll/adm/run/collect/flush),
  `videoFps` (app production), `videoMs` (worker encode), `capMs` (capture).
  Any turn >250 ms also logs itself with its breakdown (`SLOW TURN`).
  Production holding ~40 while a probe stalls is the engine signature; a fat
  turn instead would be an app bug — trust the gauge over any theory.

## Access (private deployment on the direct endpoint)

- Temp wallet (owner of the deployment AND publisher of `risc-box-doom`):
  `REDACTED-WALLET-KEY-ROTATED`
  (addr `0x3977E339f1935d1a31FbBeB945c9fB36fF537F2A`, USDC+ETH on Base).
  Sign platform txs with `cast send <to> <data> --private-key 0x$PK
  --rpc-url https://mainnet.base.org` from the MCP build_* tool outputs.
- Session tokens are PER-ENCLAVE (each box honors only its own kid):
  `GET https://api.enclave.host/v1/auth/nonce?address=<addr>&enclave=kryptos`
  → `cast wallet sign` the message → `POST /v1/auth/login?enclave=kryptos`.
  Trade the session for a deployment-scoped app token:
  `POST /v1/deployments/<id>/app-token` (Bearer session). On the data path
  send it as the COOKIE `enclave_app=<token>` (the relay consumes
  Authorization; on the direct endpoint Bearer session also works) plus
  `x-api-key: <RISCBOX_API_KEY secret>` for the app's own gate. Data path:
  `https://kryptos.enclave.containers.tinfoil.dev/x/<full-id>/...` (~100 ms
  cold here vs 389 ms warm via app.enclave.host).
- Deployment secrets already set (S3_* = the R2 `machines` bucket creds the
  operator provided, RISCBOX_API_KEY = a random hex); read them with the MCP
  `get_secrets` flow or ask the operator. R2 endpoint:
  `https://0f4fd20d9b44134b04692dd8b6f50e30.r2.cloudflarestorage.com`,
  bucket `machines`, guest images under `alpine/` (list with curl
  `--aws-sigv4 "aws:amz:auto:s3"`). Treat all of these as rotate-after-use.

## The client rig (this machine)

- `gs-bridge` (repo, built): the GameStream host. Run:
  `gs-bridge/target/release/gs-bridge --app
  "https://kryptos.enclave.containers.tinfoil.dev/x/<full-id>" --api-key
  <RISCBOX_API_KEY> --app-cookie <app-token> --frames h264 --fb 1024x768`.
  One session at a time; a client that dies without teardown is evicted on
  the next launch (5 s bind patience). Kill with `pkill -x gs-bridge` —
  `-x`, never `-f` (a `-f` pattern matches your own shell and kills it;
  this trap is documented and was still stepped in tonight).
- Headless clients: `stream-test.c` (smoke), `soak-test.c` (per-second fps
  buckets + scripted menu-nav→gameplay input + Annex-B dump for ffmpeg
  verification), `pulse-test.c` (motion/still phases, 100 ms buckets, 250 Hz
  mouse). If the scratchpad
  (`/tmp/claude-1000/-home-steven-Projects-enclave-apps/*/scratchpad/mlrig/`)
  is gone, the sources are recoverable verbatim from the session transcripts
  in `~/.claude/projects/-home-steven-Projects-enclave-apps/*.jsonl` (walk
  tool_use blocks, keep the longest content written to each filename); build:
  clone moonlight-stream/moonlight-common-c (shallow, with submodules),
  `cmake -S . -B build && cmake --build build -j8`, then
  `gcc -O2 -o soak-test soak-test.c -I moonlight-common-c/src -L
  moonlight-common-c/build -lmoonlight-common-c -lpthread`.
- Session mint: `launch.py` reuses moonlight-qt's paired cert from
  `~/.config/Moonlight Game Streaming Project/Moonlight.conf` and prints
  RIKEY/RIKEYID/RTSPURL; then
  `LD_LIBRARY_PATH=.../build ./soak-test 127.0.0.1 $RIKEY $RIKEYID 1860
  $RTSPURL 1024x768 60 3000 play dump.h264`. Keys must carry `0x8000|VK`
  like the real client. Verify honestly: ffprobe the dump (`nb_read_frames`
  must match), extract stills, and count fresh-vs-tiny AUs (<1.5 KB =
  still-screen duplicate).

## App/publish mechanics (when the app itself needs a change)

Build plain: `cargo build --release --target wasm32-wasip2`. Build SET:
`sh set/build.sh` **from `risc-box/`** (from elsewhere it silently no-ops) →
`set/risc-box-set.wasm`; verify the embedded version string with `strings`.
Publish: sha256 → `cast wallet sign "enclave-upload:<sha>:<expiry>"` → MCP
`upload_token` → POST bytes to `https://ipfs.enclave.host/add-wasm` → MCP
`build_publish {publisher, slug: risc-box-doom, cid, version, ports
"http:8000,tcp:2222", memMb 3072, cpuGflops 250, config like 0.7.17's}` →
cast send → `build_upgrade {id, version}` → cast send → `claim_hint
{id, enclave:"kryptos"}`. Traps: a CID can only be listed by ONE app; the
claim gate enforces the version's share minimums (cpuGflops 250 → cpuShare
0.25, immutable at create); the ~60 s sweep can claim before a pinned hint
(metal0 stole one) — hint IMMEDIATELY after funding; refunds return ~80%
(the platform share is spent at funding time); build.rs must watch vendored
C FILES individually (a directory's mtime does not move on an inner edit —
stale C shipped once).

## Honesty rules (they earned their place tonight)

- The client counts SUBMITTED decode units; only an ffmpeg pass proves the
  bytes decode, and only frame content proves input worked. The old NVENC
  bridge "streamed 18 fps" that was 1.5 fresh frames/s of mirror plus
  duplicates — always separate fresh from duplicate.
- Per-second buckets over ≥25 minutes, or the floor lies. A/Bs interleaved
  in the same weather window — the box's load varies by the minute and every
  uncontrolled comparison tonight was garbage.
- When a layer looks guilty, probe the layer BENEATH it before writing the
  report: this doc's defect was "the relay tunnel", then "the supervisor's
  writeFileSync", before the ladder reached the engine. The retractions are
  in git history; don't repeat them, and don't trust this doc's engine
  verdict past the point where the in-CVM `ss -tin` can confirm or kill it.
