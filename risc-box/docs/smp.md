# SMP: several harts in one RISC Box

Status: design, 2026-10-14. Nothing here is implemented yet.

## Why

A cold Firefox start is ~5.1 G guest instructions. One emulated hart retires ~75 MIPS on warden-host and
~35-40 MIPS on the NucBox partition, so the start costs ~66 s locally and well over 100 s live. Firefox is
multi-process and multi-threaded; the same guest kernel and Firefox under QEMU TCG started 1.6x faster
with 4 harts than with 1 (13/9/7 s vs 8/5/5 s, headless, three launches each).

## The platform half (not in this repo)

Every per-app partition is ONE vCPU today: the policy rule `enclave-isolation-policy/1` pins `vcpus: 1`,
and the policy is part of the bundle, so of the AppID. Parallel harts need a partition with more vCPUs,
which means a new rule that the relay (measurement prediction, hv-node re-verification) and the NucBox
node (`node-bridge.mjs policyFor`) apply identically. Shield inference already gets its own policy
(4 or 16 vCPUs) by the same mechanism. Until that lands, SMP runs interleaved on one host thread,
which is correct but no faster.

## State: what is per hart, what is shared

Per hart (`Cpu` today, minus the bus): `x`, `f`, `pc`, `csr` (with `mhartid` = hart index),
privilege mode, `wfi`, LR/SC reservation, the decode cache, block heads/ops, AOT slots and verify state,
the codegen JIT state and its context block, the TLB, `satp`/addressing mode, the `mstatus` copy.

Shared (`Bus`, carved out of `Mmu`): RAM (`MemoryWrapper`), exec page marks, page generations,
the code epoch, `glob_gen`, and every device: CLINT, PLIC, UART, virtio blk/net/input/snd/gpu, OPL,
overlay, the DTB.

## Devices

- CLINT: `msip[h]` at `0x2000000 + 4h`, `mtimecmp[h]` at `0x2004000 + 8h`, one `mtime`.
  It raises MSIP/MTIP in hart h's pending word.
- PLIC: context `2h` = hart h M-mode, `2h+1` = hart h S-mode (the layout OpenSBI and Linux expect).
  Each context has its own enables, threshold and claim/complete. A claim is atomic across contexts.
- DTB: N `cpu@h` nodes, each with an `interrupt-controller`. The CLINT and PLIC
  `interrupts-extended` list every hart's (M, S) pair.

## Interrupt delivery across harts

Each hart gets an `AtomicU64` "pending from outside" word plus an arm flag. Devices and IPIs `fetch_or`
the word and set the flag. The owning hart folds the word into `mip` at its next block boundary, where
it already checks `check_interrupt`. A hart in WFI waits on the flag instead of spinning.

## Memory model

RVWMO is weaker than the host's (x86 TSO), so plain loads and stores into shared RAM are enough for
ordinary accesses. `FENCE` becomes a host fence when there is more than one hart.

- AMOs become host atomics on the RAM pointer, sized 4 or 8.
- LR records (address, value). SC is a compare-exchange from that value, the usual emulator approach.
  It allows ABA, which RISC-V software tolerates.
- Chunk materialization (first write into a Zero or Shared chunk) takes a lock and re-checks. The
  `wr` pointer table becomes atomic.
- SMC: `page_gens` and the code epoch become atomics. A store from any hart bumps them, and every hart
  revalidates its own block cache exactly as it does today.

## TLB and fences

TLBs are per hart. `SFENCE.VMA` is local; Linux shoots down remote TLBs through SBI `remote_sfence_vma`,
which OpenSBI implements with IPIs that make each target hart execute the fence. So no emulator-side
shootdown is needed beyond correct IPIs.

## Threads

Phase 1, interleaved: N harts on the main thread, round-robin, a slice of a few thousand instructions
each. This proves the boot (OpenSBI HSM, Linux SMP bring-up), the devices and the snapshot format.

Phase 2, parallel: hart 0 stays on the main thread (it services the devices, as today). Harts 1..N-1
run on SET worker threads (`pthread_create` -> `thread.spawn-indirect`, as `worker.rs` already does).
JIT tables belong to the thread that compiled them, so each hart keeps its own JIT state, budget and
context block. That is also N times the per-view codegen budget.

## Snapshots

The format gains a hart count and one register/CSR block per hart. A snapshot is restored only into a
machine with the same hart count; it is part of the identity, just as the kernel and fs hashes are.
