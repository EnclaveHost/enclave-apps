# The engine freeze: found, proven, fixed (2026-08-20)

Status of the defect `engine-freeze-handoff.md` and `tunnel-stall-handoff.md`
chased: **root-caused and fixed in the engine, locally verified; fleet
activation pending the operator's toolchain rebuild.** Everything below is
measured; the raw logs live in the session scratchpad
(`repro/` under the 2026-08-20 session) and the numbers are restated here.

## The local repro (the handoff's "unverified" hope — it works)

The plain-vs-SET pair reproduces WITHOUT the fleet, but only under CPU
contention — on 32 idle cores everything is clean, which is why it needs the
pin (the fleet equivalent is cpuShare 0.25 on a busy box):

- Boot either build locally against a minio seeded with the alpine image
  (the recipe in `encode-path-handoff.md` § "Boot locally", with
  `alpine/fw_payload.elf` + `alpine/rootfs.ext2` and the 0.7.17-shaped
  config; the SET build needs an engine with `wasmtime-set-threads.patch`
  and `-W threads,shared-everything-threads,component-model-threading,shared-memory`).
- After `/status .fps > 3`: `taskset -a -pc 0 <pid>` — ALL threads onto ONE
  core. Boot unpinned first (the 671MB fetch under a pin takes forever).
- Load: TWO held-open `/video?codec=h264&kbps=3000` streams + one warm
  keep-alive connection probing `/status` at 7/s.
- Result on the UNFIXED engine, 10 minutes, same window, one arm per core:
  **SET 59 stalls >500ms (worst 1.2s), plain 8 (worst 1.3s)** — the fleet
  differential in miniature (fleet: 14.2s vs 317ms; the local weather is
  milder, the ratio direction identical).

## The mechanism, proven with in-engine gauges

Instrumentation (now permanently in the engine, env-gated behind host env
`ENCLAVE_SOCKDBG=1`) replaced the planned in-CVM `ss -tin`/strace step and
answered it better:

- Every stall was **write-side budget starvation**: all active connections'
  `check-write` returned 0 budget simultaneously, with the p2 `WriteState`
  in `Ready` — i.e. tokio's `poll_write_ready` said Pending — for up to
  935ms, and every socket unblocked at the SAME millisecond. One runtime
  dispatch frees the whole set: exactly the "entire socket set freezes"
  shape felt on the fleet.
- A 10ms heartbeat task on the engine's tokio runtime logged **zero** gaps
  >100ms through every stall: the runtime's workers and drivers were
  scheduled and healthy. Not CPU starvation of the runtime.
- The app's own turn gauge stayed 11-18ms through every stall (the stalled
  response's own body carries `turnMaxMs` — the probe records it).
- Kernel-truth checks (zero-timeout `poll(2)`, direct nonblocking
  `send`/`recv`) disagreed with tokio's cached readiness **thousands of
  times**: in one 5-minute window, 2,098 "kernel POLLOUT while cache says
  Pending" + 64 "kernel handed N bytes the cache said weren't there".

So: wasmtime-wasi's guest-facing socket paths (`try_read`/`try_write`/
`poll_*_ready` in `crates/wasi/src/sockets/tcp.rs`) short-circuit on tokio's
edge-cached readiness without a syscall. An edge consumed at the wrong
moment never re-fires — a socket that STAYS writable generates no new edges
— so the cache lies "not ready" indefinitely about a socket the kernel would
serve, until an unrelated dispatch resyncs it. Two SET guest threads
multiply the bad interleavings (~12:1 divergence rate vs plain, matching the
stall differential); the defect exists on plain too, exactly as the fleet
A/B said. One honest loose end: `poll_write_ready(noop)` reports Pending
while `try_write` microseconds later succeeds — tokio's two readiness views
disagree with EACH OTHER, not just with the kernel; the tokio-internal
misstep behind that is unresolved (upstream-report material), the fix below
sidesteps it entirely.

## The fix

`~/Projects/enclave/wasm/wasmtime-socket-level-check.patch` (wired into
`Dockerfile.wasmtime` and the patch-check workflow, applies cleanly with or
without the SET patch): on the paths a sync guest spins on — TCP send, recv,
and both readiness polls — when the cached path says "not ready", **ask the
kernel before believing it** (direct nonblocking send/recv, or zero-timeout
`poll(2)`). The cache stays the fast path; the extra syscall happens only
where the old code stalled; wakers are always registered before the fallback
answers Ready, so async callers keep their contract. UDP and accept were
left on the old path (no stall observed there); same pattern applies if one
ever shows.

Verified locally, same rig, same pins, same load, interleaved arms:

- SET: 10-min soak **4,198/4,198 probes, 0 stalls, worst 23ms** (was 59
  stalls, worst 1.2s). Plain: **4,198/4,198, 0 stalls, worst 21ms**.
- The fallback absorbed 36k (SET) / 3k (plain) cache-vs-kernel divergences
  while doing it. `videoFps` held ~40 throughout.
- `cargo test -p wasmtime-wasi --release`: 228 passed, 3 failed — all three
  are `p1_stat_extreme_host_mtime` (filesystem mtime, tmpfs clamping), and
  fail IDENTICALLY on the pristine tree: pre-existing, not the patch.
- One 6.5s outlier occurred in an earlier soak window — exactly while a
  second 671MB guest image was being fetched+booted on the same host, with
  every engine gauge silent and the app turn at 14ms: host weather from the
  measurement's own neighbor, not the socket defect. Clean-window soaks
  (above) show none.

## What remains (in order)

1. **Operator: ship the engine.** Manual "Wasmtime Toolchain" workflow →
   new image → update the `WASMTIME_IMAGE` digest in `Dockerfile.wasm` →
   that commit rolls the fleet (measurement changes; enclaves re-attest).
   The patch is already on main and inert until then.
2. **Fleet re-cert** (the 30fps bar): flip the standing deployment back to
   `risc-box-doom:0.7.17` (SET) if it isn't, and run the 31-minute
   moonlight soak from `moonlight-30fps-handoff.md` — ≥30fps in ≥99% of
   per-second buckets over ≥25 min, zero corrupt frames, input verified by
   decoded content. Expect the clean stretches to become the whole run; the
   0.5-14s freezes should be gone. `ENCLAVE_SOCKDBG=1` on the tenant's
   wasmtime turns on the divergence gauges if anything still smells.
3. Then the optional follow-ups from the old handoff, unchanged: mouse-look
   rebuild, binary `/video` framing, SIMD128 minih264, the metal0 relay tax.
