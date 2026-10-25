# Experimental protected Vulkan prepass

The patched Mesa 25.0.7 Lavapipe ICD now replaces one exact integer compute
shader with a call to the guest-local Shield graph service. A standard Vulkan
application uses the computed buffer for a subsequent draw. The application
contains no Shield RPC calls. This is a narrow driver integration, **not a
generic shader compiler or complete masked graphics renderer**.

## Admission and execution

The initial matcher compares the complete SPIR-V module to the qualified bytes
in `shield_prepass_spirv.h`. It also requires the ordinary compute pipeline
path, the `main` entrypoint, and no specialization, extra creation structures or
pipeline flags. The eligibility flag belongs to the pipeline and is never
trusted from a serialized shader-object binary. Changed/unknown modules,
shader objects, indirect dispatch (including device-generated commands), base
dispatch and dispatches during active queries use the original CPU path.
Generated-command routing never depends on device-memory dispatch counts.
Queries retain llvmpipe accounting; tested invocation counts are 64 per dispatch.

The exact shader maps 64 rows of four uint32 words to
`(x+y, y-x, z, w)`, where each component is `(word & 255) - 128`.
The driver accepts one 64-thread workgroup, two non-dynamic storage-buffer
bindings with exactly 1,024 bytes each, and disjoint aligned address ranges.
Descriptor-buffer mode is excluded. Earlier writes finish before an owned
input snapshot is made. Eligibility does not depend on private input values.

The client zero-pads the input and public 4×4 matrix to the existing FIELD_GEMM
K=32 contract. No float is quantized. All intermediate integers are within
int32 and the existing field bound. Masking and verification remain in the
existing service/core; the driver has no worker address or raw GPU handle.
The service separately approves the exact public matrix digest:
`cba40c98e8ffaf089742d01a6cf0caabc86caeaa7d3c52cd3047906670a3e517`.
Tests approve it only in their isolated synthetic policy. No production policy
has been changed. Unknown constants remain local under the service's policy.

The only socket is `/run/shield/compute.sock`. The client checks the connected
server UID is 1001 before sending bytes, bounds framing and total I/O time,
validates compact JSON without duplicate keys, checks output types/ranges and
exact per-request counters (one exchange, 8,192 MACs, no failures), and copies outputs only after the full response
passes. An absent/refused socket before any request permits original CPU
execution. A wrong peer, malformed reply or started-request failure causes
Vulkan device loss and skips later commands; it never retries on CPU. Busy,
permission and allocation errors are also fatal in this first version. Commands
completed before the failed dispatch are not rolled back. Aliasing checks use
virtual ranges; duplicate imports of the same underlying external memory are
not identified. Explicit temporary-buffer wipes do not promise to erase every
copy in json-c or application memory.

Lavapipe's submission path previously ignored the execution function's return
value. The patch propagates failure to the Vulkan queue. Pending fence and emulated timeline waits
also check device loss on bounded condition-variable waits, including when an
application requests an infinite wait. This prevents a failure before fence
attachment from leaving that application blocked indefinitely. Pending waits
poll at most every 100 ms; normal completion still wakes them immediately.
Already-signaled waits may return device loss after the device has failed.

## Executed checks

- The ordinary Vulkan compute-to-draw fixture issued eight actual protected
  dispatches: 65,536 verified GPU multiply-accumulates, with its 2,048 checked
  scalar outputs and final frame matching the CPU reference exactly.
- With no matrix approval, eight service requests executed locally with zero
  worker connections. An unavailable service used the original CPU shader.
- A changed debug name in otherwise valid SPIR-V kept the original CPU route
  without contacting the service; this deliberately conservative matcher does
  not claim structural or semantic shader recognition.
- A malformed late output value and a wrong service UID caused device loss,
  preserved the entire destination/sentinels, and skipped later draw/readback.
- Modifying one value in an actual worker GEMM reply triggered Shield's
  verification failure, retired the service, refused retry, and reached Vulkan
  device loss with no output publication. Infinite fence waits returned in
  about 0.17 seconds in this run.
- Base dispatch, specialization, dynamic descriptors, oversized ranges,
  device-generated dispatch and active statistics queries produce the same
  reference frame without a service connection. These valid API cases run
  with Khronos validation enabled; both output streams are checked for errors.
- Infinite timeline waits, including wait-before-submit, and queue/device idle
  waits observe a malformed protected response in under one second.
- The client parser/serializer tests pass under AddressSanitizer and
  UndefinedBehaviorSanitizer, including every truncation of a valid response,
  duplicate keys, wrong types, bounds, routes and late failures.
- The reference Vulkan fixture runs with Khronos validation enabled without
  reported validation errors.

The same patched ICD now passes in a separate dm-verity-backed KVM desktop
image under UID 1000 with no capabilities, NoNewPrivs and seccomp. Its ordinary
desktop environment selects the patched Vulkan ICD and Zink. The fixture makes
eight verified GPU exchanges through a UID-checked guest socket, fixed vsock
bridge and dedicated loopback worker; 2,048 scalar results and the complete
triangle frame match the software reference exactly. Vulkan cubes and OpenGL
gears render on the actual 1280×800 desktop, with no physical GPU nodes.
Process mappings for both clients confirm the pinned patched Vulkan library.

A second KVM run corrupts the ninth actual worker exchange, after the eight
successful dispatches. Vulkan reports device loss, leaves the entire output
unchanged, skips the later draw, and returns from its infinite wait in about
0.12 seconds. The service retires; a new Vulkan process's retry fails locally
with no tenth worker connection. Desktop restart and teardown still pass.

This proves the narrow native driver path, not production admission,
attestation, OpenGL protected lowering, conformance, streaming or a performance
gain. The KVM launcher is a test adapter and the public-matrix policy is
synthetic. The device continues reporting CPU honestly; most work stays local.

## Reproduce the driver build

Start from the upstream source archive pinned in `../mesa-source.json`.
Apply `mesa-prepass.patch` with `git apply --check` and `git apply`. Copy
`shield_prepass_client.c`, `shield_prepass_client.h`, and
`shield_prepass_spirv.h` into `src/gallium/frontends/lavapipe/`. Build using the
separate Mesa tools environment and `../build-mesa.sh`. The patch additionally
requires the `json-c` development package. Its installed runtime library is a
normal guest dependency. Use a separate install prefix; do not replace host
Mesa. The source fixture, SPIR-V hash and its exact byte size are recorded in
`prepass-module.json`.

A focused independent Claude review identified generated-command admission,
query accounting and untested timeline waits. Those cases now have explicit
local routing or failure propagation and executed regression coverage. The
review covered the ICD/client, not the underlying masking implementation.

Before broader use: structural NIR recognition
and approved-operation coverage; Zink/OpenGL compute routing; complete-frame
and performance testing. Private textures, visibility and framebuffer/encoding
remain inside the VM. Ordinary floating-point game shaders are not accelerated
by this initial integer prepass.
