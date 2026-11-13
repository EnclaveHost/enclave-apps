# Protected OpenGL/Vulkan prepass driver

The separate Mesa 25.0.7 image exposes ordinary Vulkan through Lavapipe and
OpenGL through Zink. A conditional NIR proof recognizes an integer compute
prepass and routes it through the existing guest-local Shield service. Apps
use standard graphics API calls; they contain no Shield RPCs. Unsupported
shaders and graphics operations retain Mesa's CPU implementation inside the VM.
This is functional API integration with a narrow protected operation, not a
complete accelerated renderer or a performance/conformance claim.

## Compiler and runtime contract

The compiler extracts exact bounded affine transforms for 64–4,096 rows. Each input
component is `x[k] = (word[k] & 255) - 128`; output component `c` is
`sum(x[k] * matrix[k][c]) + bias[c] + row * rowCoefficient[c]` modulo 2^32.
Matrix coefficients must be integers in [-119,119]. Bias and row coefficients
may use any 32-bit bit pattern and stay inside the VM. This includes the former
fixed transform `(x+y, y-x, z, w)`. It works on a bounded normalized NIR clone;
the original CPU shader remains unchanged. It accepts equivalent arithmetic
and renamed source modules rather than matching shader bytes. The proof uses
32-bit modular affine equations, tracks raw inputs until low-byte extraction,
and rejects unsupported instructions, control flow, memory effects and layouts.
Unsigned shifts are distributed only over proven bounded, non-wrapping public
row addresses with divisible coefficients. It never examines private values
when deciding eligibility. Public low-bit masks are identities only when the
complete public address interval fits the mask. Local and workgroup IDs remain
independent proof variables: global row is `64*workgroup.x + local.x`. A shader
proved only for one workgroup retains that tighter bound.

Two descriptor layouts share the same input/output byte layout:

- Vulkan: set 0, two individual storage-buffer bindings, one vector store per row.
- Zink: set 3, binding 0, exactly two descriptors (input index 0, output index 1),
  four scalar stores covering each output component exactly once.

Contracts 1/2 identify the exact original transform; contracts 3/4 identify the
broader affine family in the Vulkan/Zink layouts respectively. All four use the
same runtime restrictions and separate service authorization.

The proof is conditional, not authorization. Every dispatch independently
requires an ordinary compute pipeline with `main`, no specialization, extra
creation structures or flags; grid X from 1 through its proof bound (1 or 64), grid Y/Z=1, block 64×1×1; exact non-dynamic,
non-mutable storage-buffer layouts; two aligned, non-overlapping ranges of
exactly `gridX*1,024` bytes; no descriptor buffers, active queries or generated commands. Shader
objects, binary state, indirect/base dispatch and other layouts stay on CPU.
The result is never trusted from serialized shader objects. The proof has
instruction, recursion, evaluation and normalization limits; exceeding them
retains CPU execution. Upstream normalization preserves valid shader semantics;
it does not promise to reproduce undefined behavior or nontermination.

Worst-case 4,096-row request and response widths are covered by client tests;
the measured service accepts at most 8 MiB per request, above the client bound
of `2048 + 4096*96` bytes. The fixed I/O deadline remains an availability bound,
not a latency guarantee for a remote worker.

Earlier writes complete before an owned input snapshot. The client pads the
4×4 transform to Shield's existing FIELD_GEMM K=32 contract; no float is
quantized. The service must separately approve the exact padded matrix digest. A successful
compiler proof does not make a shader constant public or authorize disclosure.
Unapproved matrices execute in the trusted service, without reaching a worker.
Tests authorize only their public fixture matrices in isolated synthetic policies.
The fixed fixture digest is
`cba40c98e8ffaf089742d01a6cf0caabc86caeaa7d3c52cd3047906670a3e517`;
the affine fixture digest is
`3743f9f172613ba7e24c17237aba24e330c043ac4c8bc7e6e150bb6f5635a6a5`.
Bias and row terms never enter the service request. The client validates all
bounded dot products before applying those terms with modular arithmetic and
publishing the complete output. The service splits an approved product into
64-row tiles on one registered channel with independent masking/verification.
A failed tile rejects the whole dispatch, including earlier verified tiles. No production policy
changes, private constants, camera matrices or arbitrary shader approval follow
from that test authorization.

## Protected channel and failure behavior

The driver connects only to `/run/shield/compute.sock`, checks server UID 1001
before sending bytes, bounds framing and total I/O time (15 seconds for the
complete request, including all tiles), validates canonical
response types/ranges and exact counters, and publishes results only after full
validation. Masking and verification remain in the existing service/core. The
driver has neither a worker address nor a physical GPU handle.

An absent/refused socket before sending permits original CPU execution. A wrong
peer, malformed response or failure after a request begins causes device loss,
skips later commands and never retries on CPU. Permission, busy and allocation
errors are fatal in this version. Commands completed earlier are not rolled
back. Aliasing checks use virtual ranges and do not identify duplicate imports
of the same external memory. Explicit wipes cannot erase every library/app copy.

Queue submission propagates execution failure. Pending fences and emulated
timeline waits check loss at most every 100 ms, including infinite waits and
wait-before-submit. Zink's reset query also checks the shared Vulkan loss state;
loss flags are accessed atomically. A robust OpenGL context reports reset and
subsequent commands report `GL_CONTEXT_LOST`. Applications must honor context
loss; no post-reset buffer contents are promised by the API.

## Validation

The multi-workgroup extension has host-container and separate native KVM qualification;
it has not replaced the running production image. It adds 48 proof cases for both layouts,
46 sanitized client cases, and real OpenGL/Vulkan tests at 1, 2 and 64 workgroups.
At 64 groups each API checks 131,072 scalar outputs over eight dispatches, with
512 verified GPU exchanges / 4,194,304 MACs. The 65-group and local-only row-term
controls execute on CPU without a service request. Corrupting the final tile
causes device/context loss; Vulkan checks the entire output and subsequent draw
remain untouched. The original 73 proof cases retain their prior proof results.

The new native candidate passes 11 graphics cases and nine desktop/lifecycle
checks. Four protected API fixtures check 524,288 values with 2,048 verified
exchanges / 16,777,216 MACs. Final-tile corruption causes Vulkan device loss
without publication; retired Vulkan/OpenGL retries make no worker connection.
The desktop survives retirement and stops cleanly. The test uses synthetic
admission and a fixed vsock relay, not production VBS attestation.

The following describes the earlier single-workgroup/native checkpoint.

The compiler fixtures cover equivalent modules, both descriptor forms, varied
arithmetic/centering, rejected masks, memory layouts, missing stores, private-dependent
addresses, overflow, volatile accesses, atomics and control flow. Each test
requires exactly one explicit proof result and the expected contract/reason;
compile failure never counts as successful rejection. The original 32 cases
retain contract 1/2 coverage, while 41 affine cases compare the complete
extracted matrix/bias/row against independently constructed expectations or
require rejection. Offline normalization differs from the runtime pass list;
these tests validate extraction, not runtime offload eligibility for every case.
Actual driver fixtures cover the runtime hook separately.

The standard-API driver fixtures check 2,048 scalar outputs across eight
dispatches, untouched inputs/guards and (Vulkan) an exact reference frame.
Both Vulkan and ordinary OpenGL compute use eight real masked exchanges and
65,536 verified MACs. No-approval and unavailable-service cases execute locally.
Queries, generated/base dispatch, specialization, dynamic/wide descriptors and
Zink descriptor-buffer mode remain CPU. Corrupting an actual worker GEMM reply
retires the service and produces Vulkan device loss or OpenGL context loss;
unverified output is not published. Fence/timeline/idle failure tests are bounded.
The client parser retains the original ASan/UBSan negative suite. Another 1,028
sanitized cases cover random/boundary transforms, modular wrap, local/masked
responses, late invalid results leaving output unchanged, invalid coefficients,
and byte-identical requests when only local bias/row terms change.

The separate native KVM qualification includes actual Xorg rendering, both APIs,
fixed UID-checked guest/vsock/loopback transport, lifecycle and corrupted-worker
checks. Consult the saved evidence for the exact image and library hashes; a
host-container run alone is not native qualification. Production admission,
attestation, complete graphics conformance and useful hardware acceleration for
general applications remain separate acceptance work. Isolated native Moonlight
streaming is qualified separately; it uses CPU rendering and encoding.
The device honestly reports CPU. Rasterization, textures, visibility and frame
encoding remain inside the VM; ordinary floating-point game shaders are local.

## Reproduce

Start from the archive pinned in `../mesa-source.json`. Apply
`mesa-prepass.patch` with `git apply --check` and `git apply`. Copy
`shield_prepass_client.{c,h}`, `shield_prepass_nir.{c,h}` and `shield_affine64.h` into
`src/gallium/frontends/lavapipe/`, then use `../build-mesa.sh` in the pinned tools
image with json-c development headers. Install into a separate prefix; do not
replace host Mesa. The guest needs the normal json-c runtime dependency.

For compiler fixtures, additionally apply `../qualification/nir-proof-tool.patch`
and copy `shield_prepass_nir.{c,h}` and `shield_affine64.h` into `src/compiler/spirv/`. Run
`../qualification/test-nir.py --image sha256:TOOLS_HASH --source /work/mesa-source
--build /work/build-parent --output /work/new-proof-results`.
Run `../qualification/test-affine-nir.py` with the same arguments in a fresh
output directory for the generalized extraction suite.
Run `../qualification/test-grid-nir.py --tool /work/build/mesa/src/compiler/spirv/spirv2nir
--output /work/new-grid-results` with glslangValidator and spirv-val installed.
Build `test_grid_client.c` alongside the existing client sanitizer suites.
See `../qualification/README.md` for standard-API tests. Independent Claude
reviews covered the original ICD/client and structural compiler integration;
they are scoped reviews, not a cryptographic or platform security audit.
