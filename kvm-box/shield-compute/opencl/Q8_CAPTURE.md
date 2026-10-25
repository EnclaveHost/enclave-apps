# Q8 runtime capture and authentication

Status, 2026-10-24: experimental C++ integration, tested in networkless host
containers. The default capture build executes original CPU OpenCL kernels. A separate
masked variant is now implemented and tested in `Q8_MASKED.md`; neither is in
default build recipes.
The deployed desktop remains 0.6.8-prompts.

## Implemented boundary

`SHIELD_Q8_CAPTURE` enables recognition of the two pinned Q8/float kernels and
records their 19-argument ABI. The frontend obtains storage metadata from the
runtime, validates scalar widths, bounds, aliases and the dispatch grid, clones
the kernel at enqueue time, and retains the referenced buffers and events.
It waits for dependencies before reading code, scale and activation snapshots.
The original audit used a 16-job limit. The updated queue, described in
`Q8_MASKED.md`, retains bounded pending jobs and executes dependency-ready jobs
serially to bound aggregate snapshot storage without arithmetic-changing fallback. Unsupported layouts use the ordinary CPU
path before queue insertion; queued capture failures return failed events.

The capture path reconstructs original GGUF Q8 bytes and authenticates them
against configured public model policies and calibration digests. Unknown
weights produce only `approved: false` in the trace. Activation contents,
activation hashes and allocation addresses are not logged. Root labels preserve
alias relationships within each trace record without exposing process pointers.

This is application-side admission evidence. A future masked executor must
consume the captured snapshots, independently enforce service-side policy, and
apply an explicit numerical contract. The current CPU audit executes its cloned
original kernel against retained live buffers. It must not be mistaken for a
snapshot-consuming protected executor. No automatic float quantization was added.

Capture records are written before CPU execution with phase `snapshot-validated`
and execution `original-cpu-kernel-pending`. A record establishes capture, not
successful execution; command completion and output checks establish execution.
A failed log write leaves output untouched. Once the command is queued, failure
is reported without retrying it through another execution path.

The frontend's existing integer dispatch path remains separate. Its compiler
cache now checks the operation and dispatch permission identically on first
analysis and cache hits. Q8 analysis-only plans cannot acquire integer-dispatch
permission simply by being cached. The capture test containers expose no GPU,
network or Shield service socket.

## Compiler resource-limit repair

The native process inherits an 8 MiB file-size hard limit. The source compiler
previously attempted to raise it to 32 MiB in child setup, which failed before
Clang ran and silently prevented recognition. Child resource limits now honor
both inherited soft and hard limits. Tests run real integer and Q8 analyses under
the native limit, verify that a 1 KiB limit declines analysis, and ensure inherited
limits are never raised. The compiler also handles subprocess setup failures as
analysis refusal.

## Verification

- 34 unit/regression tests pass: compiler, Q8 admission, inherited limits,
  program lifetime and dispatch-cache boundaries.
- 12 original MM/MV kernel cases pass in strict and fast-math modes: 18,694 exact
  dyadic outputs, intact guards, and 12 snapshots verified byte-for-byte by a
  test-only callback. Delayed writes and argument changes after enqueue are
  included. This is controlled-value evidence, not arbitrary-float equivalence.
- Four additional integration cases pass: failed dependency without output
  modification; released input buffers and kernel; two queued launches with
  later argument mutation; and an out-of-order queue with explicit dependencies.
  All five expected snapshots match; the out-of-order case actually ran.
- An unusable audit-log target produces a failed event with unchanged output.
- Real all-layer Qwen runs through the capture frontend. The accompanying
  receipt records timing, actual frontend/PoCL/bitcode/plugin hashes and approved
  capture counts. This is a short compatibility test, not quality acceptance or
  demonstrated acceleration.

The tests require the corrected CPU PoCL ICD at the actual loaded path
`/opt/shield/pocl/lib/libpocl.so.2`, with digest
`90e4f6a7bbbe32d6170ca9575f3ad2630d31cd21fe3427e63dcb0faf881bb2ee`.
Both probes now include the frontend library digest, preventing a changed
frontend from being confused with an earlier experiment.

## Claude contribution and reproduction

At the user's request, installed Claude Code performed a read-only review and
wrote the four failure/lifetime/ordering cases in an isolated directory. Codex
reviewed, compiled and ran that code, integrated it, and added the log-failure
regression. Claude's review identified the audit-log ordering issue. The inherited
resource-limit defect was independently reproduced and fixed by Codex. Claude's
suggestions were not treated as executed results.

Build the experimental frontend with `SHIELD_Q8_CAPTURE`, the
`q8_capture_compiler.py` helper path and `-lcrypto`. Build a separate test frontend
with `SHIELD_Q8_SNAPSHOT_TEST`; link fixture clients with `-rdynamic`. Never ship
that callback in the normal runtime. `test_q8_capture_failures.cpp` takes the
reference MM kernel path or directory; `--log-failure` runs the separate negative
case with the log path mounted as an unusable directory. Saved reproduction
scripts and exact container commands accompany the evidence.

Remaining work is masked execution through the standard API, the numerical
contract, real Qwen/SD fault/quality/performance acceptance and eventual packaging.
Protected graphics, production identity binding, publication, wallet/nucbox
rollout and final compute-menu removal remain incomplete.
