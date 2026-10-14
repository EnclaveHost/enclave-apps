# The `codegen` verb: runtime wasm compilation for guest JITs

The 2026-08-14 interpreter rework took RISC Box as far as interpretation
goes: the busy loop now spends 72% of its time doing actual instruction
work (the rest is a superblock probe and device bookkeeping that every
measured change since leaves neutral or worse), the Alpine desktop paints
in 52 s on the fleet's wasmtime, and DOOM plays. What does NOT move is
browser-class software: firefox-esr starts — no crash, given
`ramMiB: 1792` — but shows no window inside 20 minutes, because its
startup alone is on the order of 10¹¹ guest instructions and an
interpreter retires ~10⁸/s. That is a 10–50× gap, and it is not
reachable by tuning: it is the difference between interpreting a block
and executing it as machine code.

The machine code a wasm app could target exists — wasmtime compiles the
fleet's own modules with cranelift — but a wasm module cannot add code to
itself: there is no runtime codegen inside the sandbox, by design. That
is the substrate gate, and it is the platform's to open, in the same
dedicated-interface way GPU capability grows (no tenant kernels; that
trilemma is settled).

## 0. The ask, precisely

One host-side interface, two calls:

    codegen.compile(module_bytes: list<u8>) -> result<u32, err>
    codegen.drop(idx: u32)

`compile` takes a complete, valid wasm module produced by the app at
runtime, compiles it with the engine the fleet already trusts,
instantiates it **in the caller's store, importing the caller's own
linear memory**, and installs its single exported function into the
caller's indirect-function table, returning the table index. The app
then calls it like any of its own function pointers — `call_indirect`
on an index it got back. `drop` frees the slot and the compiled code.

That is the whole surface. No RISC-V knowledge host-side, no new I/O
capability, no second memory: the compiled function can touch exactly
the bytes the app could already touch, because the only thing it
imports is the app's own memory.

## 1. Why this shape and not a "translate RISC-V" verb

An earlier sketch had the host translating guest code pages itself. The
generic shape is strictly better:

- **The app owns the translation.** RISC Box compiles its own superblocks
  to wasm in ordinary portable Rust (the predecoded ops are already a
  micro-IR; emitting a wasm function per block from them is a template
  JIT, not a compiler project). The host never learns what RISC-V is.
- **Every app benefits.** golem's QEMU port, a future x86 emulator, a
  JS runtime — anything that today interprets can tier up.
- **The security review is short.** `Module::new` + validation is the
  same code path every deployed module already passes through. The
  runtime module runs under the same sandbox, same store, same fuel and
  memory limits as its creator; it imports one memory (its creator's)
  and exports one function. There is nothing new to attest: the
  measured app is unchanged, and the generated module is data — its
  EFFECTS are bounded by wasm validation exactly as the interpreter's
  effects are bounded by Rust.

## 2. Host implementation sketch (`EnclaveHost/enclave`)

The mechanism already half-exists: SET's `thread.spawn-indirect`
instantiates code over a shared memory in the same store. This verb is
the same trick minus the thread:

- `Module::new(engine, bytes)` — cranelift compile, ~ms for block-sized
  modules; reject with `err` on validation failure.
- Pre-instantiation checks: exactly one memory import (matched against
  the caller's), no other imports, one exported func of the agreed
  signature `(i32) -> i32`, table/global/start sections refused.
- Instantiate in the caller's store; `table.grow(1)` on the caller's
  funcref table; `table.set` the export; return the index.
- Meter compilation like any host verb (host CPU is host CPU); cap
  resident compiled modules per deployment (a few thousand block-sized
  functions is plenty; LRU beyond it) so a hostile app cannot hoard
  host code memory.

## 3. What RISC Box does with it

The superblock cache keeps its exact invalidation story — physical-page
tags, write-snoop generation, SFENCE-immune — and adds a third tier:

    interpret once -> build predecoded block -> (hot) emit wasm, compile,
    dispatch via call_indirect until the page's generation dies -> drop

A compiled function loads guest registers from the register file's
fixed offset in linear memory, runs its ops as wasm (loads and stores
inline the TLB fast path; a miss or trap bails back to the interpreter
with pc exact, the same contract exec_block already keeps), and returns
the retired-instruction count.

**Measured, not estimated** (`emu/examples/jit-proto.rs`: a hand-encoded
emitter for the hot-op subset, run under wasmtime-the-crate natively,
state-equivalence asserted on 199 randomized blocks before timing; a
DOOM-shaped fixed-point loop, 12 ops/iteration, 3M iterations):

    interpreter tier           247 MIPS
    call-per-BLOCK compiled    247 MIPS   (1.0x — worthless)
    call-per-REGION compiled  2070 MIPS   (8.4x)
    region + TLB probe on
      every memory access     1379 MIPS   (5.6x)

The last row is the honest one: every load and store runs the same
direct-mapped TLB-hit sequence the interpreter's fast path uses (probe,
compare, bail to the interpreter on miss), and the multiplier that
survives is 5.6x.

Coverage is measured too (`--features blockstats`, the Alpine desktop
boot's first 4.8G instructions, retired instructions bucketed by how
hot their block was):

    execs 1-3        0.9%      execs 256-4095   13.4%
    execs 4-15       1.3%      execs 4096+      79.2%
    execs 16-63      2.2%      single-step       0.1%
    execs 64-255     3.0%

92.6% of the dynamic mix runs in blocks executed 256+ times — a
compile-everything distribution. Region structure is measured as well
(block-successor edges recorded at dispatch, Tarjan SCCs over
function-local edges only — calls and returns excluded, since a region
compiler doesn't cross them): 57.4% of retired mass sits inside
function-local LOOPS, in healthily-sized units (the top regions run 3
to ~1100 blocks). The remaining 43% is hot but call-shaped.

That splits the app-side plan into two tiers with known values:
loop-region translation alone is worth ~1.9x end-to-end; reaching the
~4.6x ceiling (5.6x on 93%) needs function-granular units that reach
each other with `call_indirect` INSIDE wasm — compiled-to-compiled
calls through the app's own table, which is cheap, unlike the measured
host-dispatch boundary (1.0x). The verb surface already permits both:
a unit is just a module, and the table indices it returns are exactly
what compiled units call each other by. End state: the desktop boot in
the 7-8 s band, and firefox's 20-minutes-and-counting startup at
roughly four.

Two conclusions with teeth. First, block-granular dispatch cannot pay
for the call boundary: the translator must form REGIONS — compile a
loop's branches into internal `br_if`s so one call runs the whole loop.
(The verb needs nothing extra for this; a region is just a bigger
module.) Second, the estimated band holds under realistic memory semantics: 5.6x with per-access TLB probes, 8.4x without. The achievable end-to-end multiplier is then set by
region coverage of the dynamic mix and the inlined TLB checks, which is
exactly the app-side tiering work RISC Box owns. That lands busy
throughput in the several-hundred-MIPS band conservatively: the desktop
boot drops toward ten seconds, DOOM toward launch-speed, and a browser
stops being a different category of software from everything else this
machine runs.

## 4. State of the app side

Everything the app owns is built and equivalence-tested behind the
`jit` cargo feature (zero impact on shipped builds):

- `emu/src/jit.rs` translates real superblock ops — the full integer
  and double-precision hot set — into wasm modules, keeping
  exec_block's contract exactly (pc-exact exits, constant retired
  counts, stale-generation bails after stores).
- `emit_region` compiles block sets into one fuel-bounded function
  (br_table-in-loop lowering; in-region branches are internal
  transfers; `run(fuel, entry)` stops at a block boundary once the
  fuel is spent, so device servicing keeps its cadence).
- The TLB tier makes translated code correct under paging: memory ops
  probe the software TLB's real layout and bail to the interpreter on
  miss or stale meta.
- `form_regions` picks regions from the recorded block graph:
  function-local SCCs over near edges, heat-ranked, size-capped.

What cannot be finished before the verb exists is the last seam:
executing the emitted modules over the app's OWN memory. In
production this costs nothing by construction — the register file,
pc cell, generation counter, TLB arrays and guest DRAM all already
live in the app's linear memory, so the Layout just names their real
addresses and a compiled function touches the same bytes the
interpreter does. There is no native shortcut for that (a host
wasmtime cannot wrap the emulator's Rust heap as a wasm memory
without copying whole DRAM per call), which is one more way of
saying the verb is the right platform boundary: instantiate over the
caller's memory, and the entire tier works with zero marshalling.

## 5. The live tier (2026-10-02)

The verb exists (`enclave:codegen/compiler@0.1.0`, see
`work/wasmtime-codegen/docs/enclave-codegen.md`) and the tier is
connected behind the `codegen` cargo feature (SET wasm64 build with
`set/codegen.c` linked and codegen-componentize wiring the import). What
runs, all in `emu/src/jit.rs` and `emu/src/cpu.rs` (`JitState`):

- **Formation.** Interpreted block dispatches are sampled (one window of
  2^18 retired in 64) into heat and successor edges; every 50M retired the
  AOT's greedy formation grows regions (<= 64 blocks) from the hottest
  uncovered blocks. Excluded: blocks already in a runnable region, blocks
  whose code changed under a proof (self-modifying or guest-JIT code),
  blocks not mapped in the current address space, the other privilege
  side, untranslatable first ops, near-cold members.
- **Emission.** Regions are rebased to a page-aligned bias, so a module is
  position-independent: the same code at another address, in another
  process or another machine is the same module. Nothing about the
  machine's address is baked in — a context block read at entry carries
  the Cpu's address, the chunk read/write pointer tables, the exec-page
  marks and the bias. The import is `env.memory` as memory64, shared, with
  the app's declared maximum. Guest registers live in wasm locals for the
  call (loaded at entry, written back at one exit). Every load/store keeps
  the interpreter's fast-path contract: within one 4 KiB page, a TLB hit
  with a fresh meta, inside DRAM; stores additionally bail (pc exact,
  nothing written) when the chunk is not owned (copy-on-write is the
  interpreter's), the page is marked executable (the interpreter's store
  bumps the write-snoop generation), or the store lands in the
  framebuffer bookkeeping window. Indirect jumps to member blocks
  (returns, jump tables) stay in the region through a compare tree.
  Translated: the whole hot set plus DIV/DIVU/REM/REMU (and W forms),
  MULW, FENCE/FENCE.I.
- **Dispatch.** Exactly where the AOT splice runs baked regions: a
  verified region entered at a block-cache hit runs with fuel 256, then
  note_retire / check_interrupt as before; zero retired falls back to
  exec_block. Validity is the AOT verifier's two levels per installed
  instance — a content proof per write-snoop generation (marking member
  pages executable) and a mapping re-probe per TLB meta — with failures
  cached.
- **Budget.** The host's limits are per execution view and cumulative
  (256 modules, 1024 attempts, 16 MiB submitted, 256 KiB per module;
  drop never refunds). `jit::verb` owns one process-wide policy under
  them: 240 modules, 960 attempts, 15 MiB, 128 KiB per module; a module is
  compiled at most once (cache keyed by region source + layout) and a
  failure is never retried; a region must show sampled heat of
  max(6000, 20 x ops) — compile time measured linear in size, ~0.07 ms per
  guest op — doubled every 48 compiles; at most 4 compiles per pass; a
  missing verb, a host quota/binding failure or 6 failures in a row turn
  compilation off for the process. Compiled regions keep running; the
  rest interprets.
- **Controls.** `RISC_JIT=0` disables it at machine start; `/status` gains
  a `jit` object; `RISC_JIT_SELFTEST=<seed>[,<n>]` runs the generated-guest
  interpreter-vs-JIT comparison inside the component; `RISC_JIT_TRACE=1`
  logs formation and compiles.

Measured (production runtime 9caac5e6, one pinned core, warden-host):

    generated guest (RISC_JIT_SELFTEST, 3 seeds, ~166M instructions:
    ALU / memory over COW chunks / calls / jump-table dispatch / FP /
    self-modifying code / M-ext / page faults), state identical every seed
      interpreter                        79-83 MIPS
      JIT, whole run incl. compiles     369-470 MIPS   (4.5-5.9x)
      JIT, second half                  417-577 MIPS   (5.0-7.3x)

    browser workload (frozen Badwolf snapshot; the page's own JS and
    layout timers; checksum 863400 verified every run), 4 interleaved
    runs per variant, median [min-max] ms
                         js                    layout
      0.6.58             11738 [10933-17230]   15259 [15046-21983]
      JIT build, off     12341 [11172-12538]   15182 [15148-15382]
      JIT build, on       5944 [ 5699-12115]   16023 [15801-16306]

JS runs 2.0x faster at the median. Layout is ~5% slower: 44-47% of
the guest's instructions ran compiled, but this is a ~20-second
workload from a cold JIT, and the compiles (1.0-1.7 s per run,
synchronous on the machine's thread) land largely in the layout phase;
with compiles disabled (before the slot-presence bitmap) the bookkeeping
alone left JS unchanged and layout 1-7% slower, so it is part of the gap
too. One JIT run in four here (one of 26 across all builds measured)
showed no JS speedup (12115 ms); formation is sampled and the cause was
not caught in a trace. The 0.6.58 maxima are one run slow across the board (host
interference).

    guest CPU work over /exec on the restored desktop, 2 runs x 3 reps,
    median [min-max] ms, outputs identical across variants
                          0.6.58             JIT off            JIT on
      sha256sum 4 MiB     4313 [4246-4492]   4323 [4263-4519]    754 [662-999]     5.7x
      gzip 4 MiB          3885 [3811-4112]   3866 [3826-4070]    837 [807-1538]    4.6x
      busybox awk loop    7335 [7061-7478]   7339 [7123-7563]   3038 [2392-3872]   2.4x
      sh while loop       3944 [3869-4155]   3978 [3919-4237]   1958 [1785-2596]   2.0x

What limits it now: compiles are synchronous on the machine's own
thread (a SET worker cannot compile for it: table indices belong to the
view that compiled them), so warm-up stalls land in whatever the guest
is doing; regions still exit at untranslated ops (MULH*, AMO/LR/SC,
single-precision FP, CSR access) and at every region boundary (no
region-to-region chaining yet); and any store to any marked code page
re-proves every region (a per-page write record would keep unrelated
regions proven).

## 6. Fallback

Absent the verb, RISC Box stays as shipped: the interpreter is at its
local optimum and every path in this document degrades gracefully to it.
The verb is pure upside behind a feature probe, the way `set:true`
already gates the worker.
