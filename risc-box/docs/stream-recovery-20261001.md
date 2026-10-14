# RISC Box streaming recovery

The active deployment's terminal worked while its game appeared frozen in
both viewers. There were two separate failures:

* The local Moonlight adapter still addressed an old isolated VM and pinned
  its old transport key. The app had moved. It repeatedly failed TLS handshakes.
* The restored Doom process kept rendering a stationary world and ignored
  game input. Its `GetAdjustedTime` multiplies elapsed milliseconds by 35 in a
  signed 32-bit integer, overflowing after 61,356,675 ms (about 17 hours).
  The independent unsigned `I_GetTime` multiplication wraps at about 34 hours.
  The installed binary contains these exact instruction sequences; regression
  tests reproduce both faults. Restarting the game restored advancing frames
  and input. This is consistent with the timer fault; no frozen-process memory
  was inspected to establish its precise elapsed timer value.

## Changes deployed

The current guest runs a layout-preserving timer correction. Widen multiplication
before division in both clocks. `timer-wide.patch` applies the same fix to new
source builds. `patch-palette-timer.py` refuses any input except the exact deployed
palette binary, changes three timer instruction spans, and preserves all symbol
addresses so the renderer's baked optimization remains usable. The emulator's
instruction checks still reject baked regions whose instructions changed.

Original SHA256: `b96ad681efc98357b6987fa857ebf778e6ac21fd3602b32b9a93d47e3f17ca00`.
Corrected SHA256: `83f5803fd2530c7475d2b313cc3212ea1f3eb5d4f17ef032dd916a7e29740476`.

The owner-authorized local viewer now resolves the running VM through the
authenticated manager and independently verifies its endpoint against the
existing pinned app, runtime, release, measurement, deployment, AMD chain and
minimum TCB. Only successful fresh verification updates the bridge key/route.
The connector independently verifies again before forwarding SSH. A local
systemd timer checks for changed endpoints every 30 seconds. Unchanged inventory
does not restart services. Unsupported releases fail verification and need an
explicit trusted configuration update; metadata never selects a trusted identity.

Local deployment uses `scripts/refresh-local-viewer.mjs` and
`scripts/local-viewer-connector.mjs`, with private configuration under
`~/.config/enclave-risc-moonlight/`. No API key or pairing key is in this repo.
No host-side plaintext rendering or isolation bypass was introduced. NVENC
remains on the authorized viewing computer, after verified TLS delivery.

## Validation and recovery

* Actual RV64 old/new arithmetic executed under qemu-riscv64: both old overflow
  cases reproduced; 200,013 wide arithmetic cases passed, including unsigned
  elapsed subtraction across clock wrap. This is accelerated arithmetic testing,
  not a 34-hour production soak. Existing signed millisecond API limits beyond
  roughly 24 days are outside this correction.
* Both changed upstream source objects compile with the original RISC-V toolchain.
* Binary length/layout unchanged; only the three allowed spans differ; wrong
  input and repeated patch application refuse.
* Viewer parser rejects each missing verification assurance and ambiguous keys.
  Live independent verification passed fresh nonce, replay, app/runtime/TCB and
  deployment binding checks before route activation.
* Web framebuffer generations and pixels advance. Keyboard Escape visibly opens
  the menu. Moonlight negotiated/decoded H.264 and reported approximately 20–26
  changing source frames/s in stable samples, with lower rates during startup
  and transitions. Its ~60 encoded FPS includes duplicates, not 60 FPS gameplay.

Before recovery, the private app saved
`risc-recovery/e64f7cba-before-stream-fix-20261001.snap`. The corrected state is
also saved as `risc-recovery/e64f7cba-timer-fixed-20261001.snap` and written to
this deployment's configured `risc-perf-agent/warm960-palette.snap` startup
snapshot. The previous frozen session remains recoverable from the first copy.
The private guest keeps its original binary in `/tmp/stream-recovery/xdoom-before`.

Local evidence: `/home/steven/enclave-bench/risc-stream-recovery-20261001/`.
The ledger rollover and paid verification service are separate work; this fix
does not imply those remaining services are complete.
