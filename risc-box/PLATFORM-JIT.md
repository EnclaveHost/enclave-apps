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

## 4. Fallback

Absent the verb, RISC Box stays as shipped: the interpreter is at its
local optimum and every path in this document degrades gracefully to it.
The verb is pure upside behind a feature probe, the way `set:true`
already gates the worker.
