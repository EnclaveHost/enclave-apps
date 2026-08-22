# RESOLVED 2026-08-22 — premise did not survive verification. KEEP FOR THE RECORD.
#
# The verify-first step (below) was executed and the hypothesis is FALSE:
# wasmtime 49 (the fleet pin) already elides bounds checks on shared
# memories — Memory::can_elide_bounds_check never consults `shared`, and
# disassembly of the production module shows direct guard-page accesses in
# both builds. The "125 vs 65" that motivated this was two different code
# generations (the SET artifact was built mid-edit and still carried
# per-dispatch formation bookkeeping). Same-revision A/B truth: SET costs
# ~16%, ~2/3 of it EPOCH INTERRUPTION (the SET patch forces
# epoch_interruption(true) as its anti-DoS stop), ~1/3 the out-of-line
# VMMemoryDefinition hop + atomics-enabled guest codegen. The only engine
# lever left is leaf-function epoch-entry-check elision — safety-sensitive,
# needs its own reviewed task if ever pursued.
#
# Handoff: guard-page bounds elision for SHARED wasm memories (fleet wasmtime)

Copy-paste prompt for the session that does the engine work. Everything
below was measured on 2026-08-22 on Steven's dev box unless marked
otherwise.

---

## The prompt

Make the fleet's patched wasmtime elide bounds checks on SHARED linear
memories the same way it already does for non-shared ones (virtual-memory
reservation + guard pages), so SET (shared-everything-threads) apps stop
paying a per-memory-access tax on their hot loops.

### Why this is worth an engine patch

Measured on risc-box (the RISC-V emulator app, whose hot loop is ~all guest
loads/stores), same machine, same guest image, same demo workload:

| build                    | demo-phase guest MIPS |
|--------------------------|----------------------|
| plain wasm32-wasip2      | **125**              |
| SET (shared memory)      | **65**               |

That is a 1.9x penalty for DECLARING the memory shared. It is not lock
contention and not the worker thread working: the SET instance's worker was
idle during the measurement and the main loop takes no locks per access.
The moment the linear memory is `shared`, wasmtime compiles an explicit
bounds check into every load/store instead of relying on the guard-page
trap. Every SET app on the fleet currently chooses between "has threads"
(display workers, encoders — the whole reason SET exists) and "core loop
runs at full speed". DOOM-at-60fps had to ship as a plain build because of
this; the desktop/Moonlight build eats the tax today.

### Hypothesis to verify FIRST (30 minutes, no code)

Shared memories must declare a maximum and can never move, so the classic
`reserve max+guard, mprotect-on-grow` scheme is sound for them — this is
what the browser engines do for SharedArrayBuffer-backed wasm memories.
Wasmtime's gap is (probably) that its `SharedMemory` implementation takes
the "dynamic" memory plan, whose codegen re-checks length on every access.
Verify before writing anything:

1. Check UPSTREAM first. `git log`/issues in bytecodealliance/wasmtime for
   shared-memory + bounds-check/guard-page work. If upstream already fixed
   it, this task becomes "backport onto the fleet patch set", which is a
   different (smaller) job. The fleet's wasmtime base version is whatever
   `~/Projects/wasmtime-set` is checked out at — check its merge-base.
2. Confirm the mechanism locally: compile a trivial module twice (same
   code; memory `shared` vs not) with `wasmtime compile` from
   `~/Projects/wasmtime-set/target/release/wasmtime`, disassemble, and look
   at a load site. You should see the explicit compare+branch only in the
   shared one. This is the artifact to show shrinking to zero at the end.
3. Read where the plan is chosen: `wasmtime_environ` memory planning
   (`MemoryStyle::Static` vs `Dynamic` or their current names) and the
   bounds-check emission in the cranelift func-env (`heap_addr` /
   `bounds_check` lowering). Find the branch that forces shared memories
   off the static/guarded path.

### Where everything lives

- Engine/platform repo: `~/Projects/enclave` (supervisor, toolchain images,
  release pipeline).
- Wasmtime checkouts: `~/Projects/wasmtime` (clean-ish) and
  `~/Projects/wasmtime-set` (carries the fleet patches, has a built
  `target/release/wasmtime`).
- Existing fleet wasmtime patches to not break: the SET threads patch
  (spawn + fd-namespace) and the tokio-coop socket-readiness patch that
  shipped as v0.5.477 (upstreamed as bytecodealliance/wasmtime#14174). The
  v0.5.477 ship is the PRECEDENT for this whole pipeline: toolchain image →
  repin → releases v0.5.x/-cpu → fleet update.

### The change (sketch — trust the code over this)

- Let shared memories with an admissible declared max take the
  static/guarded memory plan: reserve `max + guard` once at instantiation
  (shared memories already never move), keep pages beyond the current
  length PROT_NONE, `mprotect` them accessible on `memory.grow`.
- Concurrent-growth semantics stay correct by construction: a racing access
  either faults on a still-protected page (trap, as before) or sees the
  newly-grown page (allowed — growth visibility is unordered in the
  threads proposal). `memory.size`/length reads keep whatever atomic they
  use today; only the per-access CHECK goes away.
- Watch the pooling allocator and any per-instance address-space budget:
  a full static reservation per shared memory may change memory-pool math
  (the fleet runs SEV-SNP guests; address space is cheap but the pool
  config may cap reservation sizes — `memory_reservation`, guard size
  knobs in the supervisor's wasmtime Config).

### Validation ladder (all rigs exist already)

1. Micro: the two-module disassembly diff above — the shared build's load
   sites lose their compare+branch.
2. Micro-perf: a 30-line memory-loop wasm (write one; nothing suitable is
   lying around) — shared within ~5% of non-shared.
3. Macro (the real gate): risc-box A/B on Steven's box. minio on :9100
   (creds riscboxtest/riscboxtest123, bucket `machines` already seeded).
   Run the SET wasm (`enclave-apps` main worktree,
   `risc-box/set/risc-box-set.wasm`, or rebuild with
   `EXTRA_FEATURES=aot sh set/build.sh`) under the patched runtime with
   `-W threads=y -W shared-memory=y -W shared-everything-threads=y
   -W component-model-threading=y -Stcp -Sinherit-network
   -Sallow-ip-name-lookup`, config pointing at the alpine desktop image,
   and read guest MIPS as an /status `instret` delta over 20s during the
   DOOM demo. Success: SET ≥ ~115 MIPS on the rig where plain does 125
   (was 65). Then the same A/B on fps: SET desktop demo within a frame or
   two of plain.
4. Regression: the SET socket behavior from v0.5.477 must survive (that
   patch gates readiness on the tokio coop budget — different layer, but
   both touch the runtime; re-run its reproducer if in doubt), plus
   whatever engine test suite the repo has, plus one eyesoff-ai smoke (the
   other big SET app).

### Ship + gotchas (from the v0.5.477 experience)

- Pipeline: toolchain image build → repin → cut releases (v0.5.x and -cpu)
  → fleet updater. kryptos can wedge mid-update ("In-progress …
  failed/relaunch" with update AND relaunch 400) — the unstick was
  fleet-op `stop` then `start --tag`; budget ~30 min for the GPU CVM
  re-registration afterward. metal0 is self-hosted and the updater never
  touches it — it needs a manual update or it keeps the old engine.
- Don't roll the fleet while a deployment someone cares about is mid-soak.

### Payoff

Every SET app gets its threads for free. risc-box specifically: the
desktop/Moonlight build (streams H.264 from a worker) would jump from ~65
to ~120+ MIPS — which puts the full 1024x768 desktop DOOM at 60fps, not
just the dedicated fb machine, and un-forks "fast or streamable".
