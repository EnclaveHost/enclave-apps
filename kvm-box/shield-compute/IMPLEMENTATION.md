# Full implementation acceptance ledger

The requested end state is a GPU-facing interface inside KVM Box with GPU work
accessible only through Enclave Shield's masked, verified channel. Native
llama.cpp and sd.cpp must run inside the confidential VM. Supported offload must
accelerate on the untrusted GPU; remaining computation stays inside the VM.
The generic compiler/device work is part of the objective, not replaced by the
integer prototype or by calling an external inference service.

Current development image: `release-v6.11-declines-candidate` (0.6.11). Earlier release
sections below are historical evidence. The acceptance checklist remains open;
neither successful narrow kernels nor passing fault tests establish general
GPU compatibility, speedup, numerical quality, or deployment readiness.

## Required evidence before completion

- [ ] Native llama.cpp generates text inside the attested VM, with measured
  masked offload and a local reference/accuracy comparison.
- [ ] Native sd.cpp generates an image inside the attested VM, with measured
  masked offload and a local reference/quality comparison.
- [ ] The guest exposes a GPU-compatible programming interface, with documented
  context, memory, module, launch, synchronization and error semantics.
- [ ] A compiler translates supported kernel/graph operations into vetted
  masked operations and executes supported remaining operations locally.
  Compatibility must be tested against real application/kernel workloads;
  an invented graph-only API is insufficient evidence for GPU compatibility.
- [ ] Public/private data classification is enforced at the trusted boundary;
  only authenticated, explicitly approved public weights can leave in clear.
- [ ] Numeric contracts preserve application semantics or explicitly opt into
  a validated approximation. No silent floating-point-to-field substitution.
- [ ] No raw GPU device, passthrough, CUDA RPC, or alternative unmasked compute
  backend is accessible through the supplied VM GPU interface.
- [ ] VM OS/broker mediation prevents bypass of the supplied local GPU channel.
  Document the exact scope; an unrestricted Internet application is not an
  information-flow sandbox and can intentionally disclose its own inputs.
- [ ] Corruption, replay, pad exhaustion, unavailable workers, invalid requests,
  concurrency and process recovery are tested at the deployed boundary.
- [ ] Secrets, pads and verification state stay in confidential VM memory;
  storage and any shared-memory traffic meet the same threat model.
- [ ] New images are separate and measured, with authenticated model storage,
  reproducible build inputs, a rollback path and no production-service changes.
- [ ] Moonlight, FreeDoom, Fluxbox and lightweight Firefox remain functional.
- [ ] Documentation states supported GPU APIs, unsupported features, leakage,
  performance/accuracy measurements, installation, use and recovery behavior.

## Verified intermediate evidence

The initial host-side integer prototype passed 10 compiler tests, three real-GPU
transport tests and two underlying C integrity regression tests. An RTX 3070
completed a masked matrix product followed by local ReLU/multiply with an exact
CPU-reference match. Corrupt replies were rejected and identical inputs received
distinct masks. These tests do not prove CUDA compatibility, VM confinement or
native-engine integration. This was the initial v0.3 evidence; the native VM results below supersede its deployment status.

## Native integration approach

Build isolated snapshots of the pinned Enclave engine revision, its accepted
patch set, the existing sd.cpp accelerator fork and the Shield SD backend.
Use one GGML ABI (`GGML_MAX_NAME=128`), explicit CPU/Shield module loading and no
native CUDA/Vulkan backend in the trusted runtime. Authenticate model storage
and public-weight policy before admitting offload. Use dedicated test-worker
reservations on the RTX 3070, never the production V100 workers.

The compatibility frontend and VM mediation remain independent required work;
successful GGML integration does not check those items off.

## Native VM and UI evidence (2026-10-23)

The live development release is now `release-v4-ui`, with an 8 GiB SNP desktop,
read-only dm-verity public-model volume, native CPU/Shield binaries and a
separate service UID. The v3 and v4-fixed images remain available for rollback.
Native masked/CPU Qwen and SD smoke tests completed inside the attested VM;
see `native/README.md` for counters, timings and quality limitations. The
interactive guest-terminal Qwen test also succeeded with 726 offloads and zero
verification failures. No standard virtual GPU API or nvtop device exists yet.

The Run-button availability regression is fixed and deployed. Six Node tests
passed, as did seven release-policy tests. A real masked job was running when
the new page opened: it showed busy/disabled, then ready/enabled automatically
without reloading. The smoke recorded 9,486 offloads and zero verification
failures. Moonlight was re-paired and restored after the measured-image update.

## Final desktop cleanup (after virtual GPU acceptance)

- [ ] Remove the Shield Compute and Native Shield Compute menu options from the
  VM image once the standard virtual GPU interface is implemented and validated.
  Preserve the protected backend and normal application/CLI access. This is a
  user-requested final cleanup, not permission to remove the current debugging
  entry points before the replacement is ready.

## Publication phase (user-requested; after implementation acceptance)

- [ ] Complete and verify the GPU interface/compiler, boundary fault tests and
  documentation above before presenting this as a finished masked virtual GPU.
- [ ] Create the enclave application's logo and banner, package listing assets
  with the final separately measured release, and check the actual listing rules.
- [ ] Locate and validate nucbox's architecture, attestation and GPU-worker
  configuration; deploy the final release there through its supported service.
- [ ] Prepare the publication/deployment transaction using the user's wallet;
  show the exact network, app identity, permissions and cost before any required
  wallet signing approval. Never read or copy private wallet keys.
- [ ] Verify the published app identity and the running nucbox deployment,
  including native application execution and masked-only GPU access.

Publication is conditional on completing the implementation; do not publish
the current native-offload prototype as the finished virtual GPU.

## Standard API frontend progress

`opencl/` now contains a real libOpenCL-compatible development runtime backed
by PoCL CPU execution and the measured Unix service. Stock clinfo enumerates
the logical GPU. A typed Clang AST pass recognizes an initial integer GEMM
family, and ordinary OpenCL launch calls execute it through verified Shield
when public-weight policy permits. Float/transcendental kernels stay local.
Six compiler tests and six host integration modes passed, including asynchronous
user-event dependencies, exact matrix output, private-weight fallback, failed
service/dependency, busy recovery and malformed responses. The real GPU positive
control recorded one exchange and zero verification failures.

This frontend is now installed in the separately measured v5 development VM.
Its initial kernel-family coverage is not arbitrary CUDA compatibility or
complete GPU conformance.
See `opencl/README.md` for exact API behavior and remaining validation.

A targeted Khronos OpenCL-CTS selection (commit
`16b197d191903abad0047e6bcb113167b589ecb4`) now passes 16/16 host tests:
platform/device discovery, device-info comparison, context-from-type variants,
negative device/context validation, kernel enumeration/cloning, binary
creation/query, argument changes and zero-sized launches. This exposed and
fixed invalid device-type handling and context-property/failure-handle behavior.
The frontend also clears the underlying PoCL conformance-version claim; this
modified runtime is not certified. Broader CTS and VM validation remain open.

## v5 OpenCL deployment evidence (2026-10-24)

The new `release-v5-opencl` is running with the 8 GiB SNP desktop and existing
authenticated public-model volume. The v4-ui release remains available for
rollback. Stock clinfo and an ordinary OpenCL client ran inside the guest;
the service observed a verified masked exchange with an exact integer result.
Error-callback behavior was fixed in a pinned private PoCL source build after
the stock runtime failed two Khronos tests. The patched runtime passed all 30
event tests, and the packaged image passed the selected 16 API and 13 buffer
tests. Twelve host integration modes now cover sub-buffers, cloned kernels,
out-of-order queues, profiling, binary programs and multiple failure paths.
Seven compiler tests include rejection of builtin redeclarations with assembly
labels; oversized unsigned service outputs are also rejected without writes.

Native llama.cpp and sd.cpp masked/CPU checks passed in the v5 guest with zero
reported verification failures. These small workloads remain slower with
masked offload, and the SD approximation visibly changes the output. Exact
timings/quality metrics and scope are recorded in `opencl/README.md`; performance
and broader quality acceptance remain unfinished.

Moonlight repair exposed stale pairing processes and duplicate registrations
of the same client certificate, which Sunshine intentionally rejects. The
duplicate development-client registrations were backed up and cleared. Two
other saved PC entries dropped by the old newline-stripping cleanup were
restored from the private backup. `host/pair-local.py` preserves other records,
refuses concurrent pairing clients, cleans up its own pairing process and uses
an authenticated app listing as the success criterion. Its preservation tests
pass; the existing-pair verification path was exercised against the live VM.

## CUDA/HIP compiler and shared-memory progress (host tests)

`hip/` builds pinned chipStar v1.2.1 with LLVM 18 and an OpenCL-only backend.
Selected upstream HIP/CUDA applications compile and execute through the logical
OpenCL device on PoCL. The six CPU-only sample runs are compatibility results;
the newer compiled-IR masked-offload evidence is recorded below.
The glibc compatibility patch also repairs an observed host trigonometric
fraction-reduction bug; a focused host/device numerical check covers it.

The OpenCL frontend now captures SVM argument rebinding, tracks allocation
lifetimes and validates ranges/aliasing for eligible masked GEMM launches.
Ten SVM integration modes and eight Khronos SVM cases pass, along with all
twelve prior OpenCL integration modes. Actual positive controls verify masked
GPU exchanges and exact output. These changes are not deployed in v5 yet.
Broader IR lowering, profiling semantics, native applications, VM fault tests,
performance/quality acceptance and publication remain required.

## Compiled CUDA masked offload and v6 packaging (2026-10-24)

The initial integer GEMM compiled from ordinary CUDA source now reaches Shield
through standard allocation, stream and launch calls. An LLVM structural checker
compares observed linked IL with an audited reference, rejecting changed
semantics, imported-builtin substitutions, mutable globals and specialization
constants. It does not authorize public operands; the existing trusted policy
still controls that. Sixteen compiler tests (seven AST and nine IR) pass.

Nine CUDA integration modes pass against a fresh pinned HIP package, including
private fallback, visible failed-event errors with unchanged output, busy retry,
malformed/overflow replies, modified-kernel fallback, profiling fallback and
offload in both of two successive processes. The positive control records one
verified exchange, 2,048 MACs and zero verification failures. All twelve prior
OpenCL and ten SVM integration modes pass after the program-lifecycle changes.
The six CUDA/HIP CPU compatibility samples also pass with this runtime.

The selected 16 API, 30 event and eight SVM Khronos cases return pass after the
changes. Multi-device event cases cannot exercise their multi-device paths on
this single logical device; these results do not imply full conformance.

A separate v6 OCI desktop is built with the compiler, IL checker, audited
reference and a CUDA CLI sample. Container validation caught and fixed a compiler
utility's root-only permissions, libCHIP's system-library RUNPATH and cucc's
symlink-based include discovery. The packaged prebuilt CLI completes a verified
masked exchange with ordinary library discovery. This is host-container
evidence; the guest deployment evidence follows. Debug menu entries remain until
the full virtual GPU acceptance criteria are met.

## v6 deployed CUDA evidence

`release-v6-hip-xz` is now running. The first gzip build failed in firmware before
Linux because the enlarged initramfs could not be allocated. V5 and Moonlight
were restored while rebuilding with kernel-supported XZ compression. The new
desktop boot image is 535 MiB instead of 747 MiB; it has its own manifest and
predicted measurements. Both v6 SNP gates open, with host_data still unchecked
for this unpublished development deployment.

Inside the guest, uid 1000 compiled the supplied CUDA source with `cucc`, then
ran the native x86_64 program. The exact result and service counters confirm one
verified exchange / 2,048 MACs, followed by another exchange in a second process.
The deployed boundary checks report no raw GPU devices and deny direct-worker,
VSOCK and io_uring access. Moonlight pairing and video/audio startup are verified.
The v5 release remains available for rollback. This does not establish complete
CUDA coverage, performance acceptance, failed-stream recovery, or a deployed
corruption/replay/concurrency fault suite. Program-handle reuse during concurrent
release/create also needs review before concurrency acceptance.

The v6 native regression repeats the public v5 fixtures. Qwen completes with
726 offloads and zero verification failures (4.545 s masked versus 0.821 s CPU).
SD completes with 32 offloads, 288 local operations and zero verification
failures (92.132 s masked versus 27.810 s CPU). The single fixed prompt/seed's
masked-versus-CPU image comparison remains MAE 20.3003/255, PSNR 18.3974 dB,
not pixel-exact. These are smoke results, not accepted acceleration or broad
quality validation. An isolated guest profiling launch returns the exact CUDA
result while adding zero service jobs and zero verified offloads.

The v6 desktop smoke also rendered custom FreeDoom and opened Firefox with its
lightweight profile and uBlock extension. Both test applications were closed;
the protected service returned to ready, with Moonlight still running.

## Lifetime, recovery and deployed fault evidence (v6.1, 2026-10-24)

The separate 0.6.1 image adds program revision checks and metadata retirement
before native handle release. Deterministic interleavings reproduced stale IL
inheritance, lost new-program metadata and late-analysis authorization before
the fixes. Seven production-frontend lifetime tests now pass. All 22 OpenCL/SVM
integration cases, six upstream/local CUDA/HIP CPU programs, ten CUDA integration
modes and 54 selected API/event/SVM CTS cases pass. These are targeted results;
the fake lifetime compiler does not test semantic lowering, and the CTS setup
has only one logical device.

Stream destruction no longer synchronizes unrelated queues. A failed stream is
disposed while reporting its failure; healthy streams can be destroyed, and a
fresh stream can perform CPU work. The recoverable-job-rejection integration
case also completes a verified masked GEMM on a fresh stream. This does not
reset integrity retirement. The CLI status command now emits JSON on stdout,
so normal shell pipelines work without redirecting stderr.

The 0.6.1 Wasm controller and desktop passed their expected SNP measurements.
Against the dedicated RTX test worker, a guest-native CUDA positive control
returned an exact result with one verified exchange and 2,048 MACs. Replacing
the next GPU reply with the previous reply caused a visible runtime error,
left output untouched and retired the service. The two requests had distinct
masked fingerprints. Repeating the test while retired made no new worker
connection. A reboot restored a successful exact verified offload.

Separate deployed cases flipped a GPU reply bit and dropped the matrix-request
connection. Both produced visible errors, untouched output, service retirement
and successful fresh-stream CPU work. The corruption case observed one modified
reply; the disconnect case observed one dropped request and no matrix reply.
See `FAULT_TESTING.md` for the reproducible test procedure and recovery contract.
Attestation handshake replay evidence is separate from these GPU reply tests.

Deployment-specific host_data remains unchecked in this unpublished setup.
Broader kernel coverage, profiling, nvtop integration, native-engine quality
and performance, exhaustion and concurrency faults, final publication and
nucbox deployment remain open. These results do not complete the full ledger.

## Profiling implementation and host validation (v6.2, 2026-10-24)

Profiling queues no longer force every recognized kernel onto the CPU. The
frontend samples the backing ICD's device clock for logical queue, submission,
execution and completion timing. Dependency waits are excluded from execution;
input handling, service admission, masking, verification and output delivery
are included. These are logical-device timings, not physical GPU kernel times.
Unknown kernels and unsupported clocks retain the CPU path. Failed or pending
events do not expose completed profiling results.

A new callback test reproduced lost command metadata after the caller released
an event before its callback. The callback wrapper now holds a frontend/native
reference until the callback returns. Early release, failed-event callback,
late callback release and retain-inside-callback tests pass.

Nine targeted host profiling cases pass, as do the prior 22 OpenCL/SVM cases,
seven program-lifetime cases, six CPU compatibility programs, ten CUDA modes
and 54 selected CTS cases. The CUDA profiling test uses ordinary event timing
calls and requires a verified masked exchange. The packaged 0.6.2 image passes
OpenCL profiling, early callback release, CUDA profiling and recoverable-job
rejection through its installed libraries without loader environment overrides.
The CPU-only GEMM fixture now uses K=31 to select a non-offloadable shape; it no
longer depends on profiling disabling offload. See the runtime documentation
for test scope; none of these results is complete GPU conformance or speedup
acceptance.

The measured v6.2 controller and desktop then passed their SNP gates. Guest
terminal OpenCL profiling and CUDA event-timing tests each returned exact
results and added one verified exchange / 2,048 MACs. One observed logical
OpenCL execution interval was 59.164170 ms; the CUDA event interval was
565.968 ms, which also spans intervening host/runtime work. These single
observations are not comparative performance benchmarks.

A real GPU reply was corrupted during `shield-opencl-selftest profiling-failure`
inside the guest. The command exposed an error event, preserved its output and
returned profiling-info-not-available; the service retired with its successful
job/offload counters unchanged. The dedicated test worker was restored before
the recovery boot. This extends the v6.1 deployed fault evidence to profiling
queues without weakening the protected route.

After recovery, the guest CUDA profiling test again returned an exact result
with one verified exchange. Both SNP gates and the desktop boundary checks
passed; Moonlight was re-paired and its 1280x800x30 video and stereo audio stream
restored. The original dedicated worker is active and the fault proxy is stopped.
Representative native-engine accuracy/performance, broader compiled kernels,
nvtop and the remaining fault/compatibility gates are the next acceptance work.

## SD chunked masked exchanges and v6.3 (2026-10-24)

Source inspection and matched measurements identified the large SD slowdown:
the service limited exchanges to 32 rows without enabling the existing wide
batch path. Many image-token products therefore used the exact but slow local
encoded-weight fallback. The service now enables `SHIELDED_WIDE=1` for SD.
The existing C link splits each wide batch into at most 32-row exchanges, each
with fresh pads and verification. Weight authentication, arithmetic, CPU
reference mode, LLM batching and the allowed GPU route are unchanged.

In a matched host-container run with a public watercolor-cabin prompt, seed 42,
256×256, eight Euler steps, CFG 7 and two CPU threads, narrow masked SD took
87.12 s, wide masked SD 29.67 s and CPU 24.46 s. Masked results were pixel-identical;
both differed from CPU by MAE 5.75/255, PSNR 29.24 dB. The backend counted 317
offloaded and 3 local operations, versus 32/288 before. These are graph-operation
counts, not wire-chunk counts. This removes a major fallback cost but is still
slower than CPU and does not solve the numerical-quality acceptance requirement.

The packaged native backend passed five exact grouped-GEMM fixtures at 33, 64,
65, 77 and 256 rows. Repeated identical 32-row plaintext chunks had distinct
masked payloads. Corruption of the first, middle and final chunk of a 65-row
graph each left caller outputs untouched and permanently retired the backend;
repeating the graph made no additional matrix request. A real SD service test
corrupted its second chunk and required retirement without an image, successful
job increment or further work. All 15 service tests passed. Reproduction tools
and scope are in `native/README.md` and `native/test_wide.py`.

Release `release-v6.3-wide` is a separate measured development image based on
v6.2; its native SDK is unchanged. Both controller and desktop SNP gates passed.
Inside that VM, the same SD fixture took 31.21 s masked versus 24.39 s CPU,
recorded 317 offloads/3 local operations/zero verification failures, and had the
same image comparison as the host. The Qwen smoke returned Paris with 726
offloads and zero verification failures. Guest boundary checks again found no
raw GPU devices, denied direct-worker/VSOCK/io_uring access and confirmed the
local service and no_new_privs. Deployment-specific HOST_DATA is still not bound
in this unpublished setup. The new chunk-fault tests are host tests, not newly
executed SNP fault tests; prior deployed fault evidence remains in v6.1/v6.2.

Evidence: `native-wide-validation.json`, `native-wide-vm-validation-v6.3.json`,
`evidence/native-wide-host/` and `evidence/native-wide-vm/` in the output bundle.
No broader GPU API, nvtop, protected graphics, publication or nucbox acceptance
item is completed by this change. Both temporary compute menu entries remain.

The final interactive guest CLI run also completed and wrote `/tmp/wide.png`
(316 offloads, 4 local operations, zero verification failures); local fallback
counts can vary between runs. Moonlight video/audio was restored after pairing.

Follow-up found while checking these counters: the native backend's `macs`
statistic sums reconstructed graph products, including local products. The
service in 0.6.3 copied this into `maskedMacs` whenever any offload occurred.
That native telemetry field therefore overstates actual remote work and must
not be used as physical GPU utilization or verified wire-MAC evidence. Fix the
native reporting path to expose the C link's actual exchange/MAC counters and
test mixed local/remote graphs before nvtop integration. The OpenCL graph
channel's own exchange counters have a separate source. This accounting issue
does not alter the chunk-result or verification-failure checks above.

## Verified native accounting deployment (2026-10-24)

Further inspection found that the legacy C link counters also include local
products and increment received-exchange counts before verification completes.
`native/shield-verified-counters.patch` therefore adds separate verified wire
exchange/MAC counters in the C link, committed only after all members and rows
of a reply pass verification. A new GGML getter sums these counters under the
existing pool/card locks. Legacy diagnostic semantics remain unchanged.

The bootstrap now requires this API and reports `graph_macs` separately from
verified `exchanges`/`macs`. The service parses that explicit format, rejects
legacy ambiguous reports, and commits only successful-job totals. Earlier
verified chunks of a failed job remain visible to the SDK but are not published
as successful-job telemetry. The separate OpenCL graph core has not changed;
its service path already uses only successful remote calls.

The packaged 0.6.4 candidate passed 11 native fixtures, one real SD service
corruption test, one old-backend startup refusal, and 28 service/compiler tests.
Fixtures include grouped products and tails, repeated identical chunks with
distinct masks, corruption in first/middle/final chunks and in the second group
member, mixed local/remote graphs, unverified diagnostic mode and teardown
retention. The mixed fixture reports 198,656 backend graph MACs but 65,536
verified wire MACs. A rejected second member does not count the already-checked
first member as a verified exchange. Diagnostic unverified mode reports zero
verified work; production bootstrap continues to require verification.

The corrected host SD fixture produced pixels identical to 0.6.3, with 316
offloaded / 4 local operations, 1,116 verified exchanges, 178,589,270,016 verified
MACs and 181,525,282,816 graph MACs. These MAC units count logical field products,
not physical instructions, floating-point FLOPs or GPU utilization. This is a
reporting correction, not broader GPU API support or nvtop integration.

The incremental SDK rebuilds only the native backend and bootstrap; all other
pinned engine binaries remain byte-identical. Artifact hashes, patch/recipe
hashes and ELF dependency/stack checks were recorded. Source parity between the
prepared pinned context and original build was checked before applying the
new patch. The separately measured 0.6.4 image is now running locally. Both SNP
attestation gates passed; deployment-specific HOST_DATA remains unchecked in
this unpublished development setup. Host evidence is in
`native-accounting-validation.json` and `evidence/native-accounting-host/`.

Inside the attested VM, masked SD completed in 31.52 seconds versus 24.48 seconds
on CPU. Masked pixels match the prior 0.6.3 fixture exactly, retaining the CPU
reference difference (MAE 5.7467/255, PSNR 29.24 dB). It reported 1,116 verified
exchanges and 178,589,270,016 wire MACs, excluding local graph work. Native Qwen
answered the public Paris fixture with 487 verified exchanges and
14,273,495,040 wire MACs, with zero verification failures. Service totals exactly
match the two native reports. The guest boundary check passed all six isolation
assertions, including no raw GPU devices and denial of direct worker access.

The interactive CUDA source sample also returned an exact result through its
normal allocation, stream and launch APIs. Its additional completed job added
exactly one verified exchange and 2,048 MACs, confirming that the separate graph
channel still works. CUDA event duration was 617.164 ms including service work;
it is not physical GPU kernel timing. Moonlight video (1280x800, 30 FPS, H.264)
and stereo audio were restored after re-pairing the ephemeral desktop.

VM evidence is in `native-accounting-vm-validation-v6.4.json` and
`evidence/native-accounting-vm/`, including the guest-terminal screenshot.
This closes the accounting correction, not the full GPU acceptance checklist.

## Deployed admission checks (2026-10-24)

The running 0.6.4 VM rejected 18 invalid requests (including alternate worker,
environment and backend injection), retaining ready state and unchanged job and
verified-work totals after each rejection. During one real masked SD job, eight
simultaneous native contenders were rejected as busy. The admitted job completed
with exactly its own verified counters, and a later exact local graph succeeded
without a restart or additional GPU counters. The SD image hash matched the
prior 0.6.4 fixture. `test_vm_admission.py` is a reusable bounded test; evidence
is `admission-vm-validation-v6.4.json`. This covers admission serialization, not
all frontend concurrency schedules, pad exhaustion, or arbitrary workloads.

## Logical GPU monitoring deployment (2026-10-24)

`monitor/` adds a pinned nvtop fork (upstream commit
`71094a9f7dfbfe7cbe9faebfdf055e580a034283`) with a Shield vendor backend. It reads
only the measured guest service's fixed Unix status socket and validates the
service peer UID. The normal nvtop terminal and JSON snapshot modes expose the
logical GPU, ready/busy/retired state, completed jobs and verified exchange/MAC
totals. Physical utilization, VRAM, clocks, temperatures and per-process GPU
attribution remain unavailable; no hardware metrics are invented. This is a
custom nvtop build, not stock nvtop discovery, NVML, or a physical GPU device.

All 28 bounded synthetic-service fixtures passed against the packaged binary,
covering strict parsing, wrong peer identity, timeouts, exact large integers,
stale-value clearing and unavailable physical metrics. The actual ncurses UI
also passed a 120x40 pseudo-terminal check with synthetic service counts. These
are host/container tests; the deployed checks below provide separate VM evidence.
The package includes full corresponding source, recipes, licenses, dependency
versions and artifact hashes. It has no physical GPU backend or runtime library
search path. Evidence: `nvtop-validation.json`, `evidence/nvtop-host/`.

The separate 0.6.5 image is deployed locally and both SNP attestation gates passed.
The guest nvtop UI showed busy during real masked SD work; completed CPU-only
work increased Jobs without increasing Exch/MACs. After CPU SD, masked SD, Qwen
and the CUDA CLI sample, nvtop JSON showed four completed jobs, 1,604 verified
exchanges and 192,862,767,104 field MACs, exactly matching service totals. The
CUDA sample again returned an exact result and added one exchange/2,048 MACs.
Physical fields were null, with no fabricated process list. Moonlight video and
stereo audio were restored. `nvtop -p` hides plots for unavailable hardware
metrics; `nvtop --snapshot` exposes exact counters.

Native SD remained 31.56 seconds masked versus 24.73 seconds CPU; pixels matched
the prior CPU and masked fixtures respectively. Native backend/bootstrap, service
and CUDA sample binaries were checked byte-identical to 0.6.4 before deployment.
VM evidence: `nvtop-vm-validation-v6.5.json`, `evidence/nvtop-vm/`, and the two
`*-verification-v6.5.txt` attestation reports. This completes the scoped logical
monitor integration, not general CUDA/NVML compatibility, physical utilization
monitoring, native numerical/performance acceptance, protected graphics, or
publication. HOST_DATA remains unchecked in this unpublished development setup.

## Native failure policy candidate (0.6.6, 2026-10-24)

A real SD regression test against 0.6.5 reproduced silent CPU completion after
disconnecting the first masked matrix exchange. Although this did not expose
plaintext to the worker, it contradicted the documented failure contract. The
new bootstrap requires a one-way strict-offload policy before registration;
transport, resource and pad exhaustion errors latch backend failure. Public
shape refusals and supported local operations remain eligible for trusted CPU
execution. The bootstrap refuses older backends lacking the policy API.

The candidate host tests passed 11 existing wide/accounting fixtures and a real
SD corruption case, plus 11 new checks: the production maskbank boundary under
16 concurrent issuers, old-backend refusal, startup unavailability, three chunk
disconnect positions, synthetic exhaustion propagation through GGML, and four
real SD service failure cases. Failed service jobs released no image and added
no successful-job counters; repeated requests were refused. The actual boundary
test advances the production counter to its last eight windows. It does not
consume 2^40 windows. The separate exhaustion shim only tests error propagation
and is absent from the packaged SDK. Twenty-eight service/compiler tests passed.

The patch applies exactly to the prior prepared source. SDK parity confirms
only the backend, bootstrap and provenance manifests changed. Native engines,
the measured service, CUDA example and nvtop binaries remain unchanged in the
candidate root. Host receipts are in `evidence/strict-host/`. The VM checks and
deployment status are recorded separately below once verified; these host tests
alone do not close the deployed-fault acceptance gate or complete the GPU work.

The first 0.6.6 measured candidate passed both deployed native failure cases and
fresh attestation after recovery, but failed the normal masked SD positive check.
It was **not accepted**. The worker rejected the new graph's 1.8 GB reservation
while still releasing the preceding graph's reservation. Strict failure policy
exposed this transition, which the previous backend could hide with CPU fallback.
The replacement 0.6.6.1 candidate retries only connection/HELLO negotiation up to
twice (100/200 ms), before public upload or masked work. A typed link-state flag,
not parsing an untrusted error string, controls this exception. Malformed version,
upload failures, failed products and pad failures remain outside the retry path.
Dedicated proxy tests prove the retry bound and the upload boundary. Its first
full host SD check completed all 320 eligible offloads with no local matrix
fallback and 1,136 verified exchanges; repeated and deployed positives remain
separate acceptance evidence.

## Strict policy deployed and recovered (0.6.6.1, 2026-10-24)

The revised separate image passed the native unavailable-worker and first-matrix
disconnect cases inside the confidential desktop. Both released no result or
successful-job counters, retired the service, and refused a repeated request.
The disconnect proxy recorded exactly one connection and one dropped matrix
request. The dedicated worker was restored after each case; fresh controller
and desktop SNP attestations passed before testing and after recovery. This
does not claim deployed pad-counter exhaustion: the counter-boundary and
synthetic propagation evidence remains explicitly host-only.

After recovery, native CPU SD took 25.04 seconds and masked SD 30.26 seconds.
Masked SD completed all 320 eligible operations, zero local matrix fallbacks,
1,136 verified exchanges and 181,525,282,816 field MACs, with zero verification
failures. Its fixed-seed image was identical to both new host runs and the prior
0.6.5 masked fixture. The CPU comparison remains non-exact (MAE 5.7467/255,
PSNR 29.2438 dB); these are one-fixture measurements, not numerical acceptance
or a speedup claim. The two host SD jobs took 28.92 and 28.36 seconds with
identical images and counts. Native Qwen returned the expected short answer,
with 487 verified exchanges and 14,273,495,040 field MACs.

The guest CLI CUDA profiling self-test returned the exact integer GEMM result
through its allocation, stream and launch APIs (598.863 ms logical event time).
The packaged `nvtop -s` displayed ready, four completed jobs, 1,624 verified
exchanges and 195,798,779,904 field MACs, matching the service. Physical GPU
metrics remained null. Boundary checks again found no raw GPU devices, denied
direct-worker/VSOCK/io_uring access and privilege escalation, and confirmed the
local service route. Receipts are in `strict-vm-validation-v6.6.1.json` and
`evidence/strict-vm-final/`; revised host receipts are in `evidence/strict-host-final/`.

The older 0.6.6 candidate is marked rejected and retained only as failure
evidence. 0.6.5 remains available as a rollback with its documented silent CPU
fallback defect. No general compatibility, broad numerical/performance,
protected graphics, publication, or nucbox gate is closed by this release.
Deployment-specific HOST_DATA remains unchecked in the development setup.

Moonlight was re-paired to the recovered 0.6.6.1 desktop and its live client
confirmed 1280×800 at 30 FPS with two-channel audio. Controller, desktop,
dedicated worker and Moonlight units were all active at the final check.

## Explicit arithmetic contract and broader diagnostics (0.6.7, 2026-10-24)

The separate 0.6.7 development image now requires an explicit approximation
choice for native masked jobs. The CLI accepts `--approximate` or `--cpu`;
the service requires `shield-fixed-point-v1` for masked requests and uses
`native-model-v1` for CPU mode. Result metadata identifies the model, and for
masked execution the authenticated policy and calibration. The bootstrap
independently requires the fixed-point contract before backend initialization.
The UI labels masked arithmetic as approximate and submits the explicit choice.
The exact integer graph/CUDA interface is unchanged. SDK parity proves only
the bootstrap and provenance manifests changed, not the engines or backend.

Thirty service/compiler tests and six UI regression tests passed. Eleven host
contract checks cover service refusal before launch, actual bootstrap rejection
and CLI argument handling. Two full host SD jobs completed all 320 eligible
operations, zero local matrix fallbacks and 1,136 verified exchanges, with
identical pixels and no verification failures. Both VM SNP gates passed;
HOST_DATA remains unchecked in this development deployment. Twelve deployed
arithmetic refusal cases left successful-work counters unchanged and did not
retire the service. The separate admission run rejected 18 invalid requests and
eight concurrent contenders, completed the active SD job with exact accounting,
and accepted a later local graph. Host receipts are in `evidence/arithmetic-host/`;
VM receipts are in `arithmetic-vm-validation-v6.7.json` and `evidence/arithmetic-vm/`.

Native SD in the VM took 30.87 seconds masked versus 24.43 seconds CPU, with the
same output as 0.6.6.1 (CPU comparison MAE 5.7467/255, PSNR 29.2438 dB). Qwen
completed through the owner API, actual guest CLI and updated page with declared
arithmetic metadata. The actual CUDA profiling CLI returned the exact integer
result in 707.208 ms logical event time. `nvtop -s` matched the service's eight
completed jobs, 3,734 verified exchanges and 405,871,052,800 field MACs; physical
utilization and VRAM remained null. Boundary checks found no raw GPU devices and
confirmed denied direct-worker/VSOCK/io_uring access and privilege escalation.
Moonlight was re-paired and confirmed 1280×800 at 30 FPS with stereo audio.

Broader host diagnostics use the unchanged 0.6.6.1 arithmetic implementation:
four authored public text passages (260 predicted positions) gave aggregate
perplexity 15.363 CPU versus 16.030 masked (+4.34%), with 92.69% top-1 agreement.
Four SD pairs had pixel MAE 3.98–13.03/255 and PSNR 20.16–27.60 dB. They differ
visibly, and both modes fail some prompt details. These small diagnostics are
not representative quality acceptance. Masked execution remains slower than
CPU for all these fixtures. See `native/README.md`,
`native-numerical-validation.json` and `evidence/native-numerics/` for method,
provenance, images and full measurements. Consent alone does not validate the
approximation; numerical quality and performance remain unfinished requirements.

This milestone does not close general workload/compiler compatibility, deployed
pad exhaustion, protected graphics, production attestation binding, publication,
nucbox deployment or final menu cleanup. The goal remains active.

## Actual allocator exhaustion through the deployed VM boundary (2026-10-24)

A separately measured, explicitly test-only image reduced the real mask bank's
fixed issuance capacity from 2^40 to 16. Its source diff adds only that limit and
a diagnostic at the existing exhausted-counter branch; the service forwards
two fixed diagnostic messages. It does not inject an error return, advance the
counter artificially, change pad generation, disable verification, or alter
native engines, bootstrap, public policies or models. It is retained as
`release-v6.7-exhaustion-test/` with a `TEST_ONLY.txt` warning and is not the
current desktop or a publishable application image.

The host test observed real counter exhaustion at 16 and GGML retirement with
`SH_ERR_EXHAUST` (-11). In the deployed fixture, both SNP attestations passed,
then a CPU Qwen positive control completed. A masked SD request subsequently
failed without releasing an image, text, outputs or stats. The service retired
and rejected both another masked request and an explicit CPU request. Completed
jobs stayed at the prior nonzero count of one; successful GPU counters stayed
zero. The actual guest terminal showed `issued=16 capacity=16` and backend
retirement with -11 in the service log. Its `nvtop -s` independently showed
retired state and the same counts. See `evidence/exhaustion-vm/allocator-witness.png`
and the owner-API `failure.json`; host source diffs/parity are under
`evidence/exhaustion-host/`.

The unchanged 0.6.7 image was restored, both fresh attestations passed, and
native CPU/masked SD and Qwen succeeded. SD took 24.58 seconds CPU and 30.46
seconds masked; the masked job completed all 320 eligible operations and 1,136
verified exchanges with zero verification failures and unchanged pixels.
Moonlight was restored at 1280×800/30 FPS with stereo audio. The dedicated
worker's systemd invocation remained unchanged throughout; production workers
were untouched. Recovery evidence is in `exhaustion-recovery-vm-v6.7.json` and
`evidence/exhaustion-vm/recovery/`.

The recovered guest also passed the exact CUDA-source profiling self-test
(669.312 ms logical event time). Its `nvtop -s` matched ready state, four jobs,
1,624 verified exchanges and 195,798,779,904 field MACs. The combined checkpoint
is `allocator-exhaustion-validation.json`. The test controller and desktop
units are inactive; the normal 0.6.7 units and Moonlight remain running.

The first launch harness sent a JSON body to an empty-body-only controller
endpoint. The controller rejected it before the desktop started. The harness
was corrected and resumed against that same test controller; this was not an
allocator-test result. The final exported harness uses the correct empty POST
and a bounded diagnostic-observation interval with automatic normal-image recovery.

This closes the narrow real-allocator exhaustion/retirement/recovery scenario
at the deployed native SD boundary. Combined with the separate production
2^40 counter-boundary test (16 concurrent issuers, eight remaining windows), it
strengthens the earlier synthetic propagation evidence. It does not prove a
2^40-window deployed session, every pool scheduling race, or exhaustion across
all GPU API paths. The full fault/API, numerical/performance and publication
acceptance requirements remain open; the goal is not complete.

## Native prompt boundary and bounded output (2026-10-24)

Inspection of the pinned llama CLI found that its initial `-p`/`-f` prompt passed
through interactive slash-command dispatch. A public synthetic `/read` prompt
opened its named test file and produced output until the reproduction's 1 MiB
file-size cap stopped it. No private file was used. The first 2 GiB container
attempt was killed without usable proof; the successful reproduction used a
4 GiB container. This does not establish a model memory defect.

`llama-literal-prompt.patch` makes supplied prompt text bypass interactive
command dispatch. The incremental SDK replaces only `libllama-cli-impl.so` and
its provenance manifest; all native arithmetic, bootstrap, masking and backend
libraries remain byte-identical to 0.6.7. Patch application and ELF checks pass.
`prepare.py` applies and hashes the patch in clean source preparations.

The service also sets an inherited 8 MiB hard per-file limit before creating
request threads. A stricter parent hard limit is preserved. The real-process
socket test observes kernel EFBIG at exactly 8 MiB and 1 MiB with a synthetic
native executable. Both controls complete before overflow; failed jobs release
no result, retire the service, preserve prior successful counters and remove
temporary files. Repeated requests cannot launch a new native child. This is a
per-file bound, not a general disk, memory or socket-output quota.

The actual packaged Qwen test passes eight service requests: CPU `/read`,
`/glob`, `/exit`, `/clear`, `/regen` and a normal prompt, plus masked `/read` and
`/glob`. Every request reaches generation. Kernel inotify witnesses, themselves
checked with file/directory positive controls, observe no synthetic file or
directory opens. Both masked jobs report 487 verified exchanges and no
verification failures. Direct `-p` remains literal; interactive `/exit` still
exits without generation. Thirty service/compiler tests and six UI tests pass.
Host evidence is under `evidence/prompts-host/`. The separate 0.6.8 measured
image passed fresh controller and desktop SNP gates. HOST_DATA remains unchecked
in this unpublished development setup. Extracted service and SDK files match
the tested artifacts, and all release file hashes were checked before boot.

Inside the attested VM, CPU SD completed in 24.58 seconds and masked SD in
30.67 seconds. The masked job completed 320 offloads, no local matrix fallback,
1,136 verified exchanges and zero verification failures; both CPU and masked
PNGs are byte-identical to the preceding fixed fixture. Normal Qwen passed.
Seven owner-API slash-prompt cases completed without retiring the service.

The actual desktop terminal then ran `test_prompt_guest.py`: ordinary-user
inotify watches observed synthetic file and directory positive controls, then
no opens during CPU and masked `/read` and `/glob` requests through the guest
CLI. Both masked cases completed 487 verified exchanges with no failures.
The test used only a world-readable synthetic public canary, removed afterward.
The native CUDA profiling example returned an exact result (611.531 ms logical
event time). Guest `nvtop -s` matched ready state, 15 completed jobs, 3,572
verified exchanges and 260,711,270,400 field MACs; physical metrics remain null.
Screenshots, the test source and API receipts are in `evidence/prompts-vm/`.

Moonlight was re-paired and confirmed at 1280×800/30 FPS with stereo audio.
The current controller/desktop units are `77ad4df5`; the worker invocation
remained unchanged and production workers were untouched. The attempted
ordinary-user `/proc` limit inspection could not see the service, consistent
with the existing `hidepid=2` isolation. Kernel EFBIG witnesses are from the
host and packaged-service tests, not a claimed deployed overflow injection.
The release checkpoint is `prompt-boundary-validation-v6.8.json`.

This closes the tested initial-prompt command-dispatch defect and adds bounded
native output files. Earlier development images retain that defect. It does
not establish full GPU API/compiler compatibility, native numerical/performance
acceptance, protected graphics, production deployment binding, publication or
nucbox readiness. Those requirements and final menu cleanup remain open.

An exploratory host SD pad comparison found 23.909 seconds CPU, 29.419 seconds
with current masked settings, 29.565 seconds with the existing vector sampler,
30.068 seconds with batch32 refill and 29.265 seconds combining both knobs.
All masked runs retained identical PNG bytes and 1,136 verified exchanges,
320 offloaded operations and zero failures. These are single runs; unrelated
CPU work overlapped part of the experiment. They establish no convincing
speedup. Production pad settings remain unchanged. Full receipts and the
benchmark source are in `evidence/pad-performance/`.

## Real OpenCL application compatibility experiment (2026-10-24)

To move beyond the exact-integer sample, an isolated build exercised the pinned
llama.cpp OpenCL backend against the logical GPU with the real public Qwen
model. The upstream vendor gate first rejected the device. A private opt-in
patch permits the logical device only with the subgroup capabilities required
by the selected implementation; the device's vendor/name are not spoofed and
this patch authorizes no offload. Device discovery then succeeded.

Real Qwen loading exposed a missing PoCL half-image builtin. A CPU-runtime patch
implements half-image reads/writes using its existing float image operations
and explicit round-to-nearest-even stores. The baseline fails to link the test;
the patched library passes 78 numeric read/write cases across six image shapes.
This is a generic runtime improvement, not protected GPU graphics or a new
masked operator. No texture or image leaves trusted execution through this code.

Qwen loading now advances to a missing half-precision tanh builtin and still
aborts without generating text. No application compatibility or acceleration
gate is closed. Both patches remain experimental and are absent from default
build recipes and the deployed 0.6.8 image. Next work is half-math support and
numerical validation, then real application execution and kernel lowering.
See `native/OPENCL_APPLICATION.md`, `opencl-application-progress.json` and
`evidence/opencl-application/` for reproducible inputs, all failure stages,
the successful image tests and their remaining coverage gaps. The normal VM,
Moonlight and dedicated worker remain running unchanged.


## OpenCL half math validated; compiler failure isolated (2026-10-24)

The isolated SLEEF runtime now supplies scalar/vector half tanh. All 393,216
binary16 evaluations match the double-libm reference after half rounding
(0 ULP); baseline fails with a missing symbol. Explicit float-to-half rounding
also now works for rte/rtz/rtp/rtn and widths 1/2/3/4/8/16. All 7,717,008 boundary,
random-bit and special-value cases match the Khronos host reference. These are
trusted CPU operations, not newly approved masked operators. The 78 half-image
cases still pass. The initial claim that these suites used the vectorizer-only runtime was
invalidated by the library-mount error documented below; corrected runs have
since established that result. Coverage limits are detailed in `native/OPENCL_APPLICATION.md`.

Qwen passes the missing-builtin stages but still aborts in LLVM InstCombine
while eagerly compiling a half convolution kernel. A standalone OpenCL source
build/binary-serialization reproducer proves the failure without a model.
Both strict and original fast-math compilation fail; disabling optimization
passes compilation only. The initial compiler comparisons were later invalidated: they mounted a
versioned library filename that the ICD did not load. See the correction below.
The candidate patches were never in default package recipes.
The CPU Qwen control succeeds and returns Paris. OpenCL Qwen generation and
masked application execution are not established.

All build/test processes from this iteration completed. The deployed 0.6.8
controller, desktop, worker and Moonlight remain active and unchanged. Current
evidence is `opencl-application-progress.json` and `evidence/opencl-application/`.
The full GPU API/compiler, workload accuracy/performance, protected graphics,
production binding, publishing, nucbox and final menu cleanup requirements
remain open. Next work is a compiler pipeline repair against the isolated
convolution reproducer, followed by actual Qwen execution and masked lowering.

## Corrected compiler experiment; Qwen runs through OpenCL (2026-10-24)

The prior compiler comparison mounted `libpocl.so.2.13.0`, but the packaged
image contains separate regular files and the ICD loads `libpocl.so.2`.
Those runs used the packaged library, as their recorded digest confirms.
They do not establish that the vectorizer backport or iteration candidate
failed. Historical receipts are retained and marked superseded. Half-math
repairs were genuinely tested because their bitcode mount was correct.

Mounting the actual ICD file and requiring digest
`90e4f6a7bbbe32d6170ca9575f3ad2630d31cd21fe3427e63dcb0faf881bb2ee`
proves the upstream vectorizer backport fixes the isolated compilation failure.
The wrong-library negative control exits before application launch. Removed
experimental instrumentation and the extra-iteration candidate; a clean rebuild
has the same digest. Replaying the four runtime patches reconstructs all eight
modified runtime source files exactly.

Real Qwen generates the same tested Paris response with 1/25 and 25/25 layers
assigned to OpenCL. Cold runs take 59.522 and 65.366 seconds including kernel
compilation; the CPU control takes 0.966 seconds. These are short compatibility
smokes, not model-quality acceptance or demonstrated acceleration. A verified
host-only CPU runtime executes all of this; there is no physical GPU path in
the test containers.

All 393,216 half-tanh checks, 7,717,008 float-to-half conversions and 78 image
cases pass against the actual backport. Five real convolution-kernel shapes
produce 7,492 exact half outputs with intact guards. These dyadic tests cover
execution/indexing but do not claim all floating-point values. The selected
Khronos CTS API tests pass 16/16, events pass 30/30, and the existing negative
callback-status regression passes. This is not full OpenCL conformance.

An all-layer profiling run records 5,934 launches of 16 kernels. Q8 matrix-matrix
and matrix-vector kernels account for about 79% of summed kernel time. The
PoCL 6 profiler labels raw nanosecond differences as microseconds; corrected
receipts document that units repair and preserve original output. Summed kernel
time is 4.932 seconds for this run. Native floating-point kernel lowering,
authenticated weight admission, protected execution/failure tests, numerical
acceptance and performance are next. The existing integer compiler does not
recognize these Q8/float kernels. No implicit approximation was enabled.

The normal 0.6.8 VM, controller, worker and Moonlight remain active. These patches
are still excluded from default recipes and the deployed image. Full GPU/compiler
support, SD through the standard API, protected graphics, production binding,
publishing, nucbox and final menu removal remain unfinished.

## Q8 compiler admission and original-kernel execution (2026-10-24)

Added experimental structural recognition of the two real Q8/float OpenCL
kernels, conservative preprocessing checks, contiguous launch/storage validation
and exact public-tensor authentication from separated code/scale snapshots.
Plans remain explicitly non-dispatchable. None of this enables raw GPU access
or silently changes floating-point arithmetic. The C++ frontend, service and
default packaging are not yet connected to these modules.

The 23 compiler/admission tests pass. Both pinned original kernels execute in
the corrected CPU-only PoCL runtime: 12 strict/fast-mode cases, 18,694 exact
dyadic outputs, intact guards, and every recorded launch admitted by the new
layout validator. This is indexing/controlled-value evidence, not arbitrary
float equivalence or physical GPU acceleration.

The real packaged policies authenticate 170 Qwen and 955 SD Q8 tensors; altered
codes and scales are rejected. Synthetic MM/MV layouts admit 340 Qwen cases
and 1,212 SD cases. Another 698 SD MV layouts are declined because the original
kernel reads complete groups of eight rows before output guards. These include
one-row tensors that may never be actual matrix operands in SD; the probe does
not claim application execution. The first SD probe stopped on that decline;
the revised receipt records unsupported layouts separately without weakening
the checks. Full details and reproduction are in `opencl/Q8_LOWERING.md`.

The next integration must capture actual C++ runtime buffer state after events,
use exact authenticated immutable weight snapshots, enforce the selected
numerical contract and execute only through the verified masked channel.
Real standard-API workload/failure/accuracy/performance acceptance remains open.
The active controller, desktop, worker and Moonlight were rechecked active and
left unchanged. The full objective, including protected graphics, publication,
wallet/nucbox deployment and final menu cleanup, remains incomplete.

## Q8 runtime capture; Claude-assisted review and tests (2026-10-24)

An experimental C++ frontend now captures the actual 19-argument Q8 ABI, obtains
runtime storage metadata, checks layout/alias bounds, clones each kernel's queued
arguments and retains its buffers. After dependencies complete, it snapshots
codes, scales and activations and authenticates the original Q8 public weights.
Trace records exclude private contents/digests and allocation addresses. This
is capture plus original CPU execution; no Q8 masked transaction is enabled.
The future executor must consume those snapshots and independently authenticate
weights at the service, with an explicit numerical contract.

Fixed two defects exposed during integration: source-plan cache hits could bypass
the operation check used on first analysis, and the compiler tried to raise an
inherited 8 MiB file-size hard limit to 32 MiB before spawning Clang. Cached Q8 or
unknown plans cannot acquire integer-dispatch permission. Child resource limits
now respect inherited soft/hard bounds. Real child compiler tests reproduce the
native limit and verify refusal under an unusable limit.

At the user's request, installed Claude Code completed a read-only review and
wrote failure/lifetime/ordering tests in an isolated directory. Codex reviewed,
compiled and executed them. Claude identified a log-write failure after output
modification; logging now precedes CPU execution and records only a validated
snapshot with execution pending. A separate negative test proves an unusable log
fails the event while preserving output. The resource-limit issue was found and
fixed independently by Codex, not attributed to Claude's hypotheses.

Final verification: 34 compiler/admission/frontend regressions pass. Twelve
strict/fast MM/MV fixture cases produce 18,694 exact dyadic outputs with guards
and byte-verified snapshots. Four additional cases pass: failed dependency,
early buffer/kernel release, queued argument mutation, and actual out-of-order
execution. Five expected snapshots match, with no unmatched callback. The log
failure case also passes. These tests do not establish arbitrary-float accuracy.

Fresh all-layer Qwen execution passes with the same tested Paris response:
1,686 captures (1,521 MV and 165 MM), all authenticated, covering 169 distinct
public tensors. It takes 81.254 seconds including capture/helper overhead with
cached PoCL kernels. This is CPU compatibility evidence, not acceleration or
quality acceptance. Containers had no network, physical GPU or Shield service
socket. Receipts now bind the actual frontend library as well as PoCL, bitcode
and plugin hashes. The normal and test-hook frontend builds remain separate.

Evidence: `q8-capture-progress.json`, `q8-capture-progress.md`, and the 47-file
SHA-256 manifest in `evidence/q8-capture/`. Technical details and reproduction
are in `opencl/Q8_CAPTURE.md`. Earlier evidence remains historical and separate.
The default recipes and deployed 0.6.8 image are unchanged; controller, desktop,
worker and Moonlight were rechecked active. Masked standard-API Qwen/SD execution,
quality/performance, protected graphics, production identity, publication,
wallet/nucbox deployment and final menu cleanup remain unfinished.

## Standard OpenCL Q8 reaches the verified masked executor (2026-10-24)

Added a persistent native Q8 executor, guest-local broker and experimental
OpenCL bridge. The compiler and runtime still authenticate actual snapshots;
the service resolves the public tensor reference against its own measured
model/policy and the executor rechecks original mapped bytes before registration.
Existing native Shield quantization, private outliers, masking and verification
are reused. Approximate `shield-fixed-point-v1` requires explicit application
opt-in and the separate administrator-controlled experimental service flag.
Private activations use local pipes/socket messages; no raw GPU API was added.

The executor reuses its backend, retires on failures, and has planned turnover
before its request cap. Requests bound input/output to 2 MiB and 32 rows; the
frontend chunks larger operations, collecting every verified result privately
before writing application output. Only a first-chunk pre-execution decline
can retain CPU execution. Later errors have no CPU retry. Size limits and
accounting checks are enforced by the service as well as the native executor.

Claude wrote the initial C++ executor and reviewed the integration; Codex
reviewed, compiled and tested it. Review fixes include planned rollover, matching
packed-weight limits and a result-extent invariant. The existing validator
already checked complete dense grids, scalar widths, strides, batches, aliases
and byte ranges. The native backend's verified group cache legitimately serves
some nodes without a new exchange and can precompute other group members;
those must not be mislabeled as fresh GPU work or necessarily as CPU arithmetic.

Real Qwen exposed nondeterministic arithmetic routing in the audit prototype's
16-job/512 MiB reservation fallback. Replaced it with bounded pending jobs and
ICD ready callbacks feeding one worker. Jobs waiting on future events retain
metadata/references without reserving snapshot payload memory. The worker bounds
aggregate snapshot/result payloads to 512 MiB. Overflow reports a resource error.
A 40-job test behind one unopened user event proves enqueue remains nonblocking
and all jobs retain the intended route. A bounded layout-only cache avoids
repeated helper processes without caching private bytes or weight permission.

Final checks pass: 28 service/protocol tests; 34 compiler/admission/frontend tests;
12 original-kernel cases with 18,694 exact dyadic outputs; five capture lifecycle,
ordering and pressure cases with 45 expected snapshots; and the log-failure
regression. The original MM and MV OpenCL kernels produce 29,184 outputs matching
a separate native fixed-point reference with nonzero data and intact guards.
They perform two verified exchanges totaling 26,148,864 GPU multiply-accumulates.
Real corrupt-reply and disconnect tests preserve all output sentinels, retire
the service, refuse a second attempt and report no successful work.

The final all-layer Qwen run completes with the tested Paris response in 44.049
seconds. All 1,686 Q8 launches are captured and authenticated: 1,521 MV and 165 MM.
There are 1,183 layout-cache hits. The service completes 726 supported operations,
including 511 new verified exchanges and 14,478,327,808 GPU multiply-accumulates;
215 operations issue no new exchange. Another 960 take the original CPU route
before protected execution. This remains a short compatibility smoke, not broad
numerical acceptance or end-to-end acceleration; the prior native CPU control
was faster. Earlier pressure-dependent runs and their differing counts remain
historical diagnostics, not valid comparative performance evidence.

Receipts and the 73-file SHA-256 manifest are in `evidence/q8-masked/`, summarized
by `q8-masked-progress.json` and `q8-masked-progress.md`. Source documentation is
`opencl/Q8_MASKED.md`. The default recipes and deployed image remain unchanged;
the controller, desktop, worker and Moonlight were rechecked active. No production
worker was touched. The host-container tests are not confidential VM evidence.

Next: standard-API Stable Diffusion, broader numerical/performance acceptance,
multi-client admission, packaging and actual confidential VM tests. Full API
coverage, protected graphics, production binding, publication assets,
wallet/nucbox rollout and final compute-menu removal remain incomplete.


## 2026-10-24 — Claude SD fallback review and service admission

Claude Code completed a restricted read-only source review (252 seconds, 25 turns). Its review and exact test evidence are saved in the projectless task outputs at `outputs/kvm-box/claude-sd-review.md` and `outputs/kvm-box/evidence/claude-sd-review/`. Claude found framing-error retirement, missing whole-frame deadlines, multi-device callback precedence, and scheduler/segmented-graph interactions. These findings were independently checked and addressed.

Service admission now uses instance-owned bounded slots with transport backpressure; idle/malformed/reset request frames cannot retire compute and an absolute frame deadline bounds trickled input. All 34 focused service/protocol/admission tests pass. Frontend pre-connect backlog EAGAIN maps to the existing bounded admission retry; execution failure remains non-retryable. The rebuilt masked frontend SHA-256 is `8394127f5c9e0a88cb7195d030dd98123bf1352fc499f8298ce8b870ab683219`. MM and MV each match 14,592 native fixed-point reference outputs; corrupt/disconnected-worker tests preserve output sentinels and refuse retry after retirement.

Experimental `native/sd-opencl-fallback.patch` enables SD's existing scheduler plus CPU fallback for a single OpenCL device, preserves multi-device behavior and excludes segmented graphs when the scheduler is active. It is not yet in an image. The isolated candidate library SHA-256 is `cb17cf64d11f0b2596cb2c8751720aeb55a162f864ea8283336210b9426a71f2`. CPU output remains byte-identical to the original (256x256, two Euler steps, seed 42). OpenCL now passes the unsupported Q8 embedding stage but still crashes in PoCL WorkitemLoops during diffusion. A one-thread run also crashes. An attention-on-CPU diagnostic is in progress under `work/shield-compat/q8-sd/opencl-cpu-attention-v1` with a bounded 300-second child timeout. This does not establish working masked SD or GPU speedup.

Follow up on Q8 nonzero-offset view restoration and concurrent OpenCL kernel argument mutation during SD weight uploads. No deployment, publication, wallet operation, menu removal or protected graphics implementation occurred. All four existing dedicated live units were active when checked. Full completion gates remain open.


## 2026-10-24 — Standard SD masked execution and Claude-assisted placement fixes

Superseding the earlier SD crash status: the candidate now runs real Stable
Diffusion through standard OpenCL and the masked service. Single-device CPU
fallback, scoped out-of-place temporary operations, serialized weight upload,
combined CPU/OpenCL attention capabilities and correct Q8 view readback fix the
observed failures. Original CPU, modified CPU and all-CPU mixed-storage controls
produce identical images. The masked OpenCL image exactly matches the direct
native Shield SDK on the public 256x256, two-step seed-42 fixture. Both execute
392 verified exchanges and 51,921,289,216 GPU MACs. Times: original CPU 10.679s,
direct Shield 13.389s, standard OpenCL 38.405s. No end-to-end speedup is claimed.
The approximate masked result differs from CPU by MAE 7.9045/255; broader model
quality acceptance remains open.

Final SD/library/plugin digests and full evidence are recorded in
`native/STANDARD_SD_MASKED.md` and the task outputs
`standard-sd-masked-progress.{md,json}`. Applying the ordered patches reproduces
29 candidate source files byte-for-byte. All 34 service tests pass; the mixed
scheduler test produces 4,096 exact outputs; Q8 views pass 17 checks (13 fail
on baseline). Corrupt/disconnected-worker tests on the final candidate publish
no image, retire compute and refuse retry.

Claude Code proposed the Q8 view fix/test and completed a further restricted
llama placement review (27 turns, 213 seconds). Codex independently reviewed,
amended, compiled and executed the changes. The native CPU fallback profile
requires llama `--no-kv-offload` to keep persistent cache writes in CPU memory;
its all-layer Qwen probe now passes in 37.990s with 511 exchanges and
14,478,327,808 GPU MACs. Broader placement/performance suggestions remain untested.

No image deployment, production worker access, publication, wallet action,
protected graphics or menu removal occurred. All four dedicated user-service
units remain active, running 0.6.8-prompts. Actual confidential VM validation,
reproducible packaging, broad quality/API/performance coverage and production
binding/publication/nucbox acceptance remain required. The goal is incomplete.


## 2026-10-24 — Installed standard GPU applications and confidential VM acceptance

The separately measured 0.6.9-standard-candidate is now running locally. OCI
image `sha256:94c1f667a5b2023a8188e5f28cee7bcc3d27706996855b522882c89a33c9e8e3`
uses installed compiler/runtime paths and separate ordinary-app/native-service
backend discovery. Explicit `shield-fixed-point-v1` remains required for Q8
masked arithmetic. Production capture diagnostics are disabled at compile time;
service verification and accounting remain mandatory. Test audit-log failures
retain their previous fail-closed behavior.

The first installed-image test exposed a warm-cache bug: opaque application
binaries lost source authorization and the second app ran entirely on CPU.
The logical Shield backend now submits source on every process start; PoCL's
internal CPU cache remains available. SD followed by llama under the same user
now records the expected 392 and 511 verified exchanges. Both installed apps,
UID-separated native service compatibility, and corrupt/disconnected-worker
retirement/no-image/no-retry checks pass. CLI loopback is 127.0.0.1 for ordinary
desktop users and remains 127.0.0.3 for service effective UID 1001.

Claude's restricted packaging review found a pre-existing private-loopback
impersonation gap. A real nftables test in an isolated network-none container
reproduced replies from an unauthorized 127.0.0.3 listener; the additional
source-address rejection blocks them while preserving legitimate service and
desktop paths. The measured candidate contains this firewall fix.

Both controller and desktop SNP gates and replay checks pass. Ordinary desktop
UID 1000 ran `sd-cli` and `llama-cli` through OpenCL in the VM. SD completes in
55.138s with 392 exchanges / 51,921,289,216 MACs and the expected
`048b2445a2422e1fca0c8c8272557e6ab829dbcc74cfa818edf8e0c61d7abf80` image.
Qwen completes in 63.437s with 511 exchanges / 14,478,327,808 MACs. The terminal
shows both installed commands, `clinfo`'s truthful logical device and passing
results; screenshots and the exact test source are saved. These remain single
short public fixtures, not general quality or speedup acceptance.

The existing native service also passes CPU/masked eight-step SD and masked
Qwen regression checks. CPU and masked SD image hashes remain identical to
0.6.8. Guest seccomp/raw-device/direct-worker boundary checks pass. Final service
state is ready: 1,121 completed requests, 2,526 verified exchanges and
262,198,394,880 MACs. The Shield-aware nvtop fork displays logical service
counters with unavailable physical metrics marked N/A. Moonlight is active at
1280x800/30 FPS, H.264, stereo. Current units end in 9053991f; the shutdown
coordination chat was informed. The worker and production services were not
restarted or modified.

The streaming image builder hashes the exact OCI export while extracting,
removing the redundant rootfs tar. An explicit hard-link option reuses the
immutable public model volume without truncating existing files. Four focused
builder tests and seven existing release checks pass. All 73 SDK manifest files,
desktop assets, compute configuration, owner access and controller Wasm parity
are checked. The old release remains available for rollback.

Evidence is in task outputs `standard-package-progress.{json,md}`,
`evidence/standard-package/` and `evidence/standard-vm/`, with SHA-256 manifests.
The deployed image remains a development candidate: HOST_DATA is unchecked,
broad numerical/performance and general API/compiler acceptance are incomplete,
protected graphics is not implemented, and production binding, publication
assets/catalog, wallet/nucbox rollout and final menu removal remain open.
The full goal is not complete.

## 2026-10-24 — Broader standard-path numerics and Claude-assisted cost reduction

The installed 0.6.9 image remains unchanged and running. Four public eight-step
SD fixtures now match the native masked SDK PNGs byte-for-byte, each recording
1,136 verified exchanges and 181,525,282,816 MACs. Standard OpenCL takes 67–79s
versus about 24s CPU and 28–30s native masked SDK. CPU-vs-masked pixel MAE spans
3.98–13.03/255; this does not establish perceptual quality or a speedup.

Four teacher-forced Qwen texts (264 tokens, 40,111,104 logits) match exactly
between standard OpenCL and the native masked SDK. With matched attention,
CPU-KV, batch and no-repacking settings, the 260 predicted positions have
aggregate perplexity 15.429 CPU / 16.020 masked (+3.83%) and 91.15% top-1
agreement. The reasoning fixture alone has a 10.17% perplexity increase.
These small public diagnostics are not broad model-quality acceptance.

Claude completed restricted source reviews of weight placement and Q8 costs.
The controlled placement experiment finds no weight/bias split inputs, identical
model-buffer totals and verified work with or without an attention CPU override.
No loader change was made. Initial timings were confounded by compiler and
verified-group caches; the corrected test warms the compiler cache and resets
the broker between processes. Its times are 36.11s versus 35.84s.

The source candidate replaces the Python per-float finiteness loop with NumPy's
CPU classification over explicit little-endian float32 bytes. Fresh validation,
weight authentication, arithmetic and fail-closed behavior remain required.
196,608 IEEE-754 classification comparisons plus maximum-frame and edge checks
pass; all 37 service/protocol/admission tests pass. Ordinary SD/llama and native
SDK checks pass with the isolated host service override. Corrupt/disconnected
worker tests publish no image, retire the service and refuse retries. This
optimization is NOT in the deployed image and needs measured packaging/CVM
validation. Its synthetic 2 MiB validator timing improves from 25.5ms to 0.042ms;
that is not an end-to-end performance claim.

See native/STANDARD_NUMERICS.md and the task's standard-numerical-progress.json,
122-file evidence manifest and standard-image-comparison.html. Public logits
are removed after analysis; output hashes and statistics remain. Production
services, the dedicated worker invocation, Moonlight and the live VM are intact.
The full goal remains active: performance/quality and wider API acceptance,
production identity binding, publication/wallet/nucbox, protected graphics and
final compute-menu removal remain unfinished.


## 2026-10-24 — Finite validator deployed and compiler costs measured

The separate 0.6.10 image (`sha256:39359bd066f88b4fffb497fa7e2bb4579282d026be3050858ad4e23866541bf5`)
now contains the NumPy finiteness validator. Overlay comparison against 0.6.9
finds only `/shield/q8_service.py` changed. All 73 SDK files, desktop inputs,
compute configuration, owner access and controller Wasm parity checks pass.
Controller and desktop SNP verification gates open and reject replay; HOST_DATA
remains unchecked, so this is an unpublished development deployment.

Installed-package tests pass, including all 37 service/protocol/admission tests
and corrupt/disconnected-worker fail-closed tests. Actual guest terminal tests
run standard `sd-cli` and `llama-cli` as UID 1000: SD records 392 verified
exchanges / 51,921,289,216 MACs, and Qwen 511 / 14,478,327,808. The guest receipt
was exported from the terminal, rather than reconstructed from a screenshot.
Native SDK, explicit approximate arithmetic and VM isolation tests pass again.
The SDK eight-step SD run takes 30.21s masked versus 23.88s CPU; no end-to-end
speedup is established. Moonlight is streaming 1280x800 at 30 FPS, H.264 stereo.
The dedicated worker invocation and production services were preserved.

Claude's source-only compiler-cache review is complete. A test-only helper
profile of installed standard applications passes with unchanged verified work
and SD PNG. It records 494 helper invocations: 238 source analyses (119 unique
source/options requests) taking 12.003s and 256 launch-layout validations taking
7.066s. The nonoverlapping interval union is 19.069s; 234 rejected-source calls
account for 11.397s. These timings exclude interpreter startup and are not a
cache benchmark or deployment speedup. A conservative rejection-only cache is
being evaluated; it must never grant an offload or bypass runtime validation.

RISC-style vector logo/banner drafts render cleanly. They are saved in `assets/`
and task outputs but are not published. `GRAPHICS.md` records the protected-
graphics design and limitations; no protected hardware graphics implementation
is claimed. See task outputs `finite-release-progress.json`, evidence under
`finite-package/`, `finite-vm/`, and `compiler-profile/`. Prior release entries
remain historical. Performance/quality/API acceptance, production binding,
fleet/wallet/nucbox publication and final menu removal remain open.


## 2026-10-24 — Claude-assisted rejection-cache prototype

Claude reviewed the measured compiler costs and then the source-rejection
prototype. The implemented optimization is opt-in at build time and disabled
in the running 0.6.10 image. It embeds exact source/options rejection hints;
hits return no lowering plan and use existing trusted CPU execution. Unknown
requests and supported Q8 kernels retain the full compiler and runtime checks.
No positive plan, tensor authorization or GPU result is cached.

A four-run ABBA host comparison uses two CPU cores, 8 GiB, fresh application
compiler caches and fresh services. All runs preserve the SD PNG hash and
Qwen generated response, with identical 392/511 verified exchange counts and
51,921,289,216 / 14,478,327,808 MAC counts. Mean SD time is 46.445s baseline /
40.861s candidate (12.02% lower); mean Qwen time is 37.056s / 29.265s (21.02%
lower). This is a small startup-heavy standard-path comparison, not a CPU
speedup, general GPU performance claim, or confidential-VM benchmark.

Claude identified the risk of baking compiler failures into permanent rejection
hints. The hardened generator now checks six positive kernel canaries before
and after generation, records installed compiler/toolchain/header/resource-limit
provenance, rejects conflicting outcomes, and hashes the bytes actually parsed.
Compile-time checks reject malformed/unsorted/duplicate entries and a mismatched
compiler entry point. Missing Clang, unreadable references, a 1 KiB file limit
and a forced compiler timeout each refuse to produce a table. Positive canaries
cannot rule out every source-specific resource failure; false declines remain
a performance/numerical regression risk, never an offload grant.

833 exact-match/mutation checks, cross-language empty/UTF-8/NUL/field-boundary
vectors, four compile-time rejection checks and three portable regression test
methods pass. The hardened frontend then passes ordinary SD/Qwen/native SDK
integration plus corrupt/disconnected-worker tests: no image published, service
retired, retries refused. The review's statement that Q8 plans marked
`dispatchable: false` have no numerical effect is incorrect: these analysis
plans feed the separately validated masked Q8 execution path. False declines
can change approximate arithmetic to native CPU arithmetic, so output checks
and verified-work counters remain mandatory.

`opencl/SOURCE_DECLINES.md` documents the prototype. Before it can be released,
packaging must enforce manifest/header/binary/source/toolchain binding, then the
new measured image needs guest validation. The 199-file evidence manifest is
in task outputs `evidence/source-declines/`; `claude-assisted-progress.md` is the
short report. Controller, desktop, worker and Moonlight remain active; the
worker invocation is unchanged. The broader full-implementation goal remains
open, including publication/wallet/nucbox and final menu cleanup.


## 2026-10-24 — Bound rejection hints deployed in 0.6.11

The separate compiler-rejection candidate is deployed and validated inside the
SEV-SNP VM. Claude's source review led to stronger canary checks, sandbox probes,
CPU-loader provenance and mandatory release metadata. The cache only selects
trusted CPU fallback; it never approves GPU work or caches tensor authorization.

Ordinary guest commands passed as UID 1000: `sd-cli` recorded 392 verified
exchanges / 51,921,289,216 MACs in 44.114 seconds; `llama-cli` recorded 511 /
14,478,327,808 in 55.87 seconds. The SD output matches the prior standard fixture.
Native SDK output hashes also match the previous release, and its boundary test
finds no raw GPU devices and rejects direct worker/VSOCK/io_uring access.

The package gate passed 16 mutation tests, eight canary-contract cases, three
installed-file tamper cases, two missing-metadata cases, and real-versus-fake
sandbox probes. The installed image reclassified all 119 source requests twice,
with six positive canaries before and after, at both 8 MiB and 32 MiB file limits.
Corrupt and disconnected worker tests publish no image and refuse retries.
All 198 SDK manifest files and preserved desktop/compute inputs were verified.

Controller and desktop attestation/replay checks pass. HOST_DATA is still
unchecked, so this remains an unpublished development deployment. Moonlight is
active at 1280×800, 30 FPS, H.264 stereo; the dedicated worker was not restarted.

There is no demonstrated end-to-end speedup over CPU: the native SDK eight-step
SD check took 30.16 seconds masked versus 23.63 seconds CPU. Earlier host ABBA
cache measurements (12% lower SD / 21% lower Qwen elapsed time) compare standard
compiler startup paths, not CPU versus GPU or a controlled VM benchmark.
General API/compiler and numerical acceptance, production account binding,
wallet/nucbox publication and final menu cleanup remain open. Graphics remains
a documented design, not implemented hardware acceleration.

Evidence: `declines-release-progress.json`, `evidence/declines-release/manifest.json`,
and `evidence/declines-vm/manifest.json`. Historical prototype reports remain
historical; this report supersedes their deployment status.


## 2026-10-24 — Explicit deployment binding and verified NucBox target

The SNP broker now launches both guest roles with Enclave's raw deployment-ID
bytes as HOST_DATA, requires the verifier to confirm that check, and the owner
connector checks its own expected ID before forwarding credentials or data.
Unbound startup requires an explicit development flag. Invalid, conflicting,
abbreviated and duplicate binding arguments are refused; symlinked startup is
covered. The hardware-tested source hashes match the exported implementation.

Eight binding regression tests, seven existing release checks, and ten hardware
cases pass. Isolated controller guests demonstrate correct-ID acceptance,
wrong/absent-ID rejection, fresh nonces/replay rejection, and refusal even when
host metadata falsely claims the expected ID. Injecting wrong/missing QEMU
labels also makes the broker itself refuse and tear down the guest. Tests use
synthetic IDs and a public invalid password; they are not wallet deployments.

Claude reviewed the change. Accepted startup/parser findings were fixed and
retested; the exported response distinguishes genuine defects from hypothetical
Moonlight import side effects and preexisting concerns.

The live 0.6.11 desktop remains an unbound development deployment. Its controller,
desktop, worker and Moonlight services remain active; the worker invocation is
unchanged. No production or NucBox service was modified.

Read-only SSH inventory confirms NucBox K11, Ryzen 9 8945HS and Windows 11 Pro.
The current public fleet row names nucbox-k11-4, Hyper-V partitions, a Radeon
780M masked worker and per-partition TLS. It advertises host-OS exclusion while
trusting the hypervisor/physical operator; these declarations are not themselves
an independent verification of the protections. The target needs a separate
measured Shield desktop profile and the existing partition verifier. An EPYC
SNP image cannot simply be published as a compatible NucBox app. Existing native
compute build flags use GGML_NATIVE=OFF and AVX2, but the full image/kernel/runtime
and worker protocol still need target validation.

Wallet authorization, actual ledger/lease binding, profile admission, remote
routing, broader compute acceptance, publication and final menu removal remain
open. No end-to-end GPU speedup over CPU or protected graphics is claimed.

Evidence: `evidence/deployment-binding/manifest.json`; protocol notes:
`deployment-binding.md`; machine-readable status: `deployment-binding-progress.json`.
The live public inventory source is https://api.enclave.host/enclaves; the saved
row includes its checkedAt time. Keep this dated evidence separate from future
fleet state.


## 2026-10-24 — Claude-assisted desktop contract and NucBox kernel preparation

Two isolated Enclave source worktrees preserve the shared checkout. The newer
V6/V7 guest implementation was recovered at
`d09f377d03cc4032df43800adb1046551b667691`; the separate `c6f5beec3` snapshot has
newer host code but lacks those guest configuration/secret/TPM changes. No broad
branch merge or production update was performed.

Claude's architecture review correctly identified that the old contracts cannot
combine the required sealed owner credentials with masked GPU compute. Its
CPU-only phase proposal was not adopted: native llama.cpp/sd.cpp, masked-only
GPU access, Moonlight and desktop parity remain required. The older host-signed
T0 conclusion was also limited to the source supplied to that review; it is not
a qualification of the recovered guest-owned TPM path.

An experimental V8 desktop contract now has Go and JavaScript implementations,
checked against Claude's independent Python reference. All 29 accepted and 230
refused vectors agree, along with twelve direct manifest refusal cases. The
contract commits a fixed masked companion descriptor, GPU share, public config,
resource minimums, Moonlight destinations and sealed-secret deployment identity.
The descriptor commits pre-image inputs; final image/runtime release pins remain
separate requirements. Unknown devices, commands and unmasked options are refused.
The integration aligned newline and JSON-depth handling. Legacy vectors pass.

This is offline identity tooling. V8 is not advertised by the manager, normal
runtime parsing refuses these bundles, and the current image cannot launch them.
No native companion, secret release, worker reservation or wallet authorization
is established merely by these passing vectors. Implementation of those runtime
paths and hardware tests remains required.

An isolated NucBox kernel compiled as `6.6.87.2-enclave-kvm-box`, SHA-256
`b11b5be7374fd6ea3f3c8c3203ffd785c2ba5c95c3ac8d2774db5da4bd0e7c15`.
UHID, uinput, evdev, generic HID, configfs, firewall rejection and the pinned guest
TPM transport are built in; static config/link checks pass. Build inputs, command,
tool versions and config changes are recorded. This is a development build from
a recovered local source tree, not an upstream reproduction or hardware-qualified
release. Claude identified preparation-helper provenance/isolation weaknesses;
the replacement helper passed thirty tests and real preparation of a fresh copy.
It now binds the complete source manifest, owns the copy, isolates its command
environment, checks exact config drift and journals failures. A second kernel
build is running from those checked inputs; its completion and hardware tests
remain pending. The earlier build is not retroactively credited with these checks.

The existing 0.6.11 controller, desktop, worker and Moonlight services remain
running. The candidate kernel has not booted on NucBox. Native profile lifecycle,
model storage, secure transport, remote routing, numerical/performance acceptance,
wallet deployment, publication and final menu cleanup remain open.


## 2026-10-24 — Wasip2 controller and first NucBox kernel probe

This checkpoint supersedes the earlier pending kernel/controller status.
Claude completed a separate `shield-desktop` controller, dashboard and 30 native
socket tests. All 30 pass after integration. The controller requires the sealed
owner password, strips owner/forwarding credentials before native proxying,
limits HTTP framing and buffers, validates WebSocket and Moonlight upgrades,
and multiplexes streams without production threads. Integration bounded the
fixed backend connect to 250 ms. Eight checks of the actual Wasip2 component
under Wasmtime 49.0.3 pass, including a 2 MiB body, six concurrent tunnels,
half-close, refusals, timeout and a saturated accept queue. These use a fixture
companion in a private namespace; no full desktop or secret release is implied.
See `host/hyperv/CONTROLLER.md` for reproduction and remaining integration.

The hardened kernel build completed with SHA-256
`f401ccc30759c48c29a63bdb36e9cd7b09b840e863016080716b644939870da8`.
A minimal diagnostic image rebuilt identically twice after the builder
reproduced its pinned baseline. Five mutations and a debug twin changed boot
measurements. A separate 1-vCPU, 2-GiB VBS/OpenHCL VM booted that diagnostic on
NucBox. UID 1000 opened uinput/UHID but was denied TPM, AF_VSOCK and io_uring;
the application seccomp program was installed. Two fresh guest-generated
bindings returned through the guest TPM, and their envelope, claims hashes,
bindings, VTL settings and exact image measurement parsed correctly.

Report signature/IDKS hardware-chain authentication was not performed. The
reports give guest SVN 0 despite recipe SVN 13. The subsequent pinned-builder
experiment below explains why the recipe field is not hardware rollback policy. This probe did not run
Moonlight, inject input, load models or exercise the GPU. Its VM was removed
by exact ID/ownership marker and its per-run guest state archived. The blank
master was not given to the VM. Production services and running VMs were not
modified. No application was published and no wallet deployment was made.

The M3 init now requires an explicit built-in TPM marker for the new kernel
release, avoiding silent omission of its report device; 18 selector checks pass.
Rootless Xorg also reached 1280×800 locally with UID 1000, no effective
capabilities, NoNewPrivs and seccomp in an isolated namespace with no host GPU,
input or TPM devices. Dynamic input handling and full desktop isolation remain.

Runtime admission is still disabled. Remaining work includes native companion
lifecycle and UID/network confinement, profile/image/runtime verification,
sealed-secret delivery, masked worker reservation/transport, authenticated
model storage, remote routing, broader compute/numerical/performance acceptance,
NucBox desktop/Moonlight acceptance, wallet deployment, publication and final
menu removal. These incremental checks do not establish a GPU speedup or a
universal protected graphics implementation.


## Native companion control path and Claude integration (2026-10-24)

The root C lifecycle supervisor and fixed UID nftables policy pass five real
namespace/pidfd/Unix-credential tests and seven packet-level tests. These cover
escaped process-group descendants, supervisor death, readiness failures, peer
identity, gateway UID gates, stopped-service impersonation and raw GPU TCP
denial. The actual desktop nsinit is not implemented; fixtures are explicit.

Claude supplied the unprivileged Go control gateway. Review corrected an
escaped-JSON test fixture and hardened cancellation so a later deadline cannot
overwrite cancellation. Twenty-three race-enabled tests, go vet and the static
build pass. The Wasm controller now forwards the validated action header needed
by the gateway; its 30 native and eight actual Wasip2 tests pass.

Five combined-chain checks run the production Wasip2 component as UID 7501,
production Go gateway as UID 1002, root C supervisor with its test-only kernel
check bypass, and actual nftables. Owner/action checks, start/stop/restart,
direct gateway denial, missing-desktop failure, owner-header stripping and
WebSocket/Moonlight duplex half-close pass. Namespace init and the inner
HTTP/stream endpoint remain fixtures. No production services were changed.

A pinned-builder SVN 13-to-14 experiment changed the IGVM file hash and
endorsement metadata but preserved the exact boot measurement. Its dump has
no VbsMeasurement directive. Recipe SVN therefore cannot establish guest
rollback enforcement; explicit authenticated measurement/release policy is
still required. Signature/hardware-chain verification remains outstanding.

Evidence is exported under outputs/kvm-box/evidence/native-companion, separate
from the previous controller/kernel checkpoint. Runtime admission, real native
desktop launch, device/input isolation, sealed secrets, masked worker and model
storage integration, compute quality/performance acceptance, NucBox desktop and
Moonlight acceptance, wallet deployment, publication and menu removal remain.


## Native boundary review and confined input helper (2026-10-24)

Claude completed an independent static review. Verified fixes expand nft UID
checks to all fixed services and the front-only owner controller, deny unlisted
UIDs, reset inherited supervisor signals, retain executable/stdio descriptors
correctly when stdio starts closed, and make child mounts private. Seven real
supervisor tests and nine actual packet tests pass, including UDP service
impersonation refusal. The domain root must be a mount point before launch.

The controller consistently refuses WebSocket queries, forces nosniff and
same-origin framing headers, and displaces idle unauthenticated header sockets
when capacity is full. Thirty-two native tests, nine real Wasip2 checks and five
combined-chain checks pass. The gateway's 23 race-enabled tests remain passing.
Same-origin desktop assets must be served by measured UID 1003 code; this
integration invariant is still outstanding. Stale supervisor sockets remain a
fail-closed condition requiring complete domain teardown/recreation.

A new fixed udev launcher retains five capabilities, with NET_ADMIN usable only
through AF_UNIX or NETLINK_KOBJECT_UEVENT sockets. Seccomp denies IP, vsock,
packet, other netlink, ioctl, mount/namespace, BPF, io_uring and related privileged
paths. Actual syscall/exec-inheritance and private multicast checks pass. The
real packaged udev daemon started under that confinement in a private namespace
with no host input devices. Hotplug, Xorg delivery and Moonlight input remain
unqualified. Exclusive desktop-guest admission and a carefully limited guest
input device tree are required before enabling this native profile.

A bzImage was built for the upcoming isolated local KVM input probe. Its
underlying vmlinux is unchanged, and stripping debug information reproduces
the exact previously probed NucBox ELF. This is preparation, not a new boot or
hardware qualification. No live desktop, worker, wallet or publication changed.


## Native input and process-role qualification — 2026-10-24

Three disposable local KVM boots passed real uinput → confined udev → rootless
Xorg keyboard event delivery without XTest. The final run uses the production
role policy (UID 1000, all capability sets zero, NoNewPrivs, pinned 71-rule M3
seccomp). Udev now has only four capabilities, mask 0x100b; MKNOD is removed
and mknod/mknodat are denied. Input nodes come from guest devtmpfs. The VM
has no NIC, host input/GPU passthrough, shared filesystem or disk.

Four process-role tests pass, including setuid execution that cannot recover
root, syscall denials, threads and CPU JIT. Five combined-chain tests now run
the real Go gateway under that role policy. Native root preparation contributed
by Claude compiles; its restricted filesystem/device layout is under separate
real-VM qualification. These results do not establish full nsinit readiness,
Moonlight input, NucBox desktop, masked GPU acceptance, attestation or speedup.

The subsequent local KVM root-layout probe passes with the exact production
header. It verifies four negative cases, private/RO mounts, descriptor cleanup,
PTY/UHID access, worker access only as UID 1001 with reciprocal peer credentials,
and real rootless Xorg keyboard input through the restricted device view.
Model and worker sources are synthetic fixtures. Full nsinit, measured home
seeding, Moonlight, authenticated storage/worker reservation and runtime
admission remain incomplete. Evidence includes the reconstructible overlay,
serial log, failed fixture iterations, production source hashes and scope.


## Independent Claude root review integrated — 2026-10-24

Claude completed its restricted static review. The main agent implemented
read-only procfs controls and fixed device metadata, nodev /dev, removal of
udev DAC_OVERRIDE (now 0x1009), namespace/source checks and mode-0600 PTYs.
Eight real-KVM rejection/fault cases, four roles, denied UHID/control/metadata
access and cross-role terminal writes, late-failure rollback, and actual
uinput/udev/libinput/Xorg input pass. The real daemon startup and syscall/exec
policy checks also pass. Root/role headers compile with -D_GNU_SOURCE and strict
warnings; existing role adapters rebuild byte-identically. Review, dispositions,
source hashes, serial logs and reconstructible overlay are exported.

This does not complete full native nsinit, home seeding, Moonlight, authenticated
resources, GPU acceptance, runtime admission, publication or wallet deployment.
The existing live desktop/controller/worker/Moonlight services remain active.


## Default-home and authenticated desktop integration — 2026-10-24

Implemented bounded compiled home seeding from the exact 14-file/15745-byte
v6.11 defaults, with build-time digest/tree validation and runtime private
tmpfs/UID/mode/empty-home checks. Eight build-input test groups pass. Runtime
refuses nonempty/wrong-owner/symlinked/RO/repeated seeds without overwriting
existing files. The actual KVM guest independently verifies every content hash.

Implemented private runtime Xauthority generation and added bounded cookie
support to the custom direct-X11 FreeDoom client. Explicit invalid authority
fails closed; legacy absent-XAUTHORITY images retain their behavior. Seven
parser test groups pass. Original desktop source hashes are preserved in a
reference snapshot; the native adaptation has separate provenance.

The composed KVM probe passes authenticated Xorg (no abstract listener),
unauthorized-client refusal, real keyboard input, Fluxbox, xterm controlling
PTY, Firefox window, FreeDoom window and active PulseAudio stream. Fixed guest
/dev/tty and /dev/full were added with read-only device metadata after the
terminal exposed the missing tty path. Eight root failure cases and four roles
also pass. QEMU exits 0, guest poweroff at 46.12 seconds. No host GPU/input/NIC
or shared filesystem is passed. Exact sources, logs and reconstructible initrd
overlay are exported in evidence/native-home.

Full nsinit/readiness, inner gateway integration, authenticated model/worker
resources, native runtime admission, GPU acceptance, Moonlight/NucBox,
publication and wallet deployment remain unfinished. Claude is implementing
the fixed inner gateway from copied public source context; it is not yet
reviewed or accepted. Existing live services remain active and unchanged.

## Fixed static UI staging — 2026-10-24

Implemented bounded asset staging for the new inner gateway's read-only image
tree. All noVNC lookups are kernel-confined beneath a pinned root descriptor;
relative aliases are flattened and escapes, cycles and special files rejected.
Eight focused test groups pass. The staged bundle contains 121 content files,
1,301,357 bytes, with per-file hashes. Packaging must assign root ownership and
a read-only mount; build modes and hashes alone do not establish admission.

The native compute page now defaults to CPU/native arithmetic. Approximate
masked GPU arithmetic requires an explicit selection. The inline script parses
and the selected default was checked. Source, exact bundle, tests and hash
manifest are exported under evidence/native-ui. Live services are unchanged;
the full integration and acceptance items above remain open.

## Claude-contributed inner gateway — 2026-10-24

Reviewed and integrated the fixed UID-1003 Go inner gateway: root-owned RO
static assets, framed native-compute Unix socket with SO_PEERCRED checks,
strict WebSocket forwarding, fixed-port Moonlight TCP/UDP tunnels and sanitized
Sunshine pairing. Twenty-five test groups pass with the race detector. The main
agent added bell audio MIME support, corrected a truncated UDP test to signal
EOF and added pairing cancellation/deadline coverage. Static build and vet pass.

Five additional checks use the production binary and role policy with actual
nft rules in disposable mapped namespaces. They verify RO enforcement, compute
peer identity, wrong-UID rejection, WebSocket early bytes/half-close and tunnel
framing. Backends are fixtures; the first harness cleanup-order error was
corrected, and the final run exits 0. No live service or release changed.

Full nsinit and whole-chain integration, real Sunshine/Moonlight, authenticated
model/worker resources, runtime admission, masked GPU quality/performance,
NucBox acceptance, publication and wallet deployment remain incomplete.

## Worker peer identity and separate SDK staging — 2026-10-24

Implemented connected Unix SO_PEERCRED UID-0 validation before protocol traffic.
The existing ggml backend already supported Unix transport; the older standalone
core did not. Each was patched against its own recorded source baseline. The
backend preserves 12 non-transport object files; only shielded-wire.c changes
in the standalone core's source manifest. Full SDK source preparation now applies
the backend patch; build-core.py has an explicit legacy-core backport option.

Six real peer-credential groups pass for each selected library: the root broker
is accepted, non-root desktop/service/UI impostors are rejected despite root
socket-file ownership, invalid paths fail, and 100 refusals leak no descriptors.
Both staged libraries load in the existing Debian guest image as UID 1001.
Compared with the deployed core, 317440 finite weight encodings, 65536 half
conversions and 12 synthetic prepared matrices are identical. The postmortem
self-test passes. No GPU execution or model-quality/performance claim follows.

The separate SDK was compared with the exact base OCI image. Its complete
manifest covers 417 files; only two libraries and source-provenance.json change
(plus the manifest), with 414 files retained. A fixed native service config now
uses the Unix worker route and clients [1000,1003], preserving all model/public
approvals and other execution settings. Eight policy-builder test groups pass.
Its hashes establish identity, not authenticated model storage or admission.

Claude's inner-gateway job completed; its implementation matches the reviewed
draft, and the final additional UDP boundary case passes. Claude completed
the native nsinit service graph draft from copied public source context; it
compiles and is undergoing composed KVM testing and a separate lifecycle review. Full lifecycle,
authenticated worker/model resources, runtime admission, GPU acceptance,
Moonlight/NucBox, publication and wallet deployment remain incomplete. The live
desktop/controller/worker/Moonlight have not been modified by this work.


## Native nsinit composition — 2026-10-24

Reviewed and integrated Claude's fixed native launcher and readiness helper.
The static build passes with warnings treated as errors. Disposable KVM tests
using the exact pinned kernel start the ten actual foreground services, check
compute/inner readiness through their assigned roles, then verify clean stop
and critical-daemon-failure teardown. Both runs verify service UIDs, udev's
limited capability set, zero capabilities for other services, NoNewPrivs and
seccomp. Host entry and unexpected arguments are refused.

The first fixture lacked unpacking memory; the next discovered CONFIG_NFT_CT=m
without the matching module. Both failures were retained. Built the exact
nft_ct source against the linked kernel's symbol CRCs, with the kernel tree
read-only in an unprivileged mount namespace. The final fixture loads this
module before installing the complete nft policy. Kernel ELF/bzImage hashes
remain unchanged, and no host module was loaded. Production packaging must
measure/supply this dependency or qualify a built-in replacement kernel.

These tests have no NIC, host devices, GPU, shared filesystem or real model
inputs. They do not establish authenticated resources, full controller-to-GPU
acceptance, performance, Moonlight streaming or runtime admission. Claude's
additional lifecycle review/test draft is still running. Existing live
controller/desktop/worker/Moonlight services remain active and unchanged.


## Combined actual native control graph — 2026-10-24

Joined the production Wasip2 controller, outer gateway, C supervisor without
kernel bypass, native nsinit, actual inner/compute services and desktop graph
in one disposable exact-kernel KVM guest. Nine checks pass: owner/action gates,
start, static/compute status routing, real VNC WebSocket traffic, Sunshine
server-info through the Moonlight tunnel, front/runtime bypass refusals, clean
stop, fresh restart and complete desktop-tree removal on supervisor death.
The final test correctly expects control 503 and desktop proxy 502 after the
supervisor is killed. Earlier wrong harness expectations are retained.

The component runs under UID7501 using a fixed Wasmtime test adapter with
capabilities removed, NoNewPrivs and app seccomp. This does not substitute for
production M3 launcher/cgroup/exclusive-guest admission. The test has no NIC,
GPU, host device, disk or shared filesystem. Model/calibration inputs and owner
credentials are synthetic; the root local worker sees three UID1001 probes
and no compute protocol payload. Full authenticated compute, numerical and
performance acceptance, Moonlight streaming, NucBox and wallet publication
remain incomplete. All live services remain unchanged.


## Claude review resolution and actual input integration — 2026-10-24

The first lifecycle test-draft job exhausted a generation limit with thinking
only, resumed automatically and delivered no files. It was cancelled when the
real KVM lifecycle tests had already covered that work. A focused medium-effort
Claude review completed successfully and found one actionable issue: ancestor
procfs could expose Sunshine's early credential argv. No other actionable
defect was reported within that review's limited source scope.

Added the checked ancestor-proc precondition before credentials: actual /proc
mount ID, every visible procfs alias, hidepid=2/invisible and no nonzero bypass
gid. Higher ancestors remain trusted platform TCB; role UIDs and group0 removal
are outer-launcher obligations. Thirteen parser cases pass. A negative actual
KVM run refuses hidepid=0 before root preparation or worker probes.

Added the minimal native input rule set with final UID/GID1000 mode0600. The
complete actual controller/desktop graph now passes eleven checks, including
outer-role argv denial and actual keyboard events through uinput, udev,
libinput and Xorg under Fluxbox. UID1001/1003 event-node opens are denied. An
initial window-manager focus race was fixed in the test popup without synthetic
X input. All prior nine lifecycle/control checks still pass.

Evidence is saved in evidence/native-proc-input. The selected nsinit binaries
now include the guard; prior binaries are historical pre-hardening checkpoints.
SquashFS tools and an isolated read-only-lower overlay build path are prepared
for the separate image. The nft module build wrapper was corrected to capture
parent namespace identities before unshare; its rebuilt module hash is unchanged.
No new measured root image, authenticated GPU resource admission, end-to-end GPU
acceptance, Moonlight streaming, NucBox deployment or wallet publication is
claimed. Live services remain unchanged.


## Separate immutable native image and CLI qualification — 2026-10-24

Built the separate 564,912,128-byte SquashFS/xz desktop root with SHA-256
15315e3669ad945f1d11133d48039c5075e2df8efaad07a16ea6b9b394798224 and dm-verity
root 0ff673f600eff89d2dc3bd84371d429862e50a67f48be3682ff228cf08a76f59.
The unprivileged packager uses private user/mount namespaces, a read-only base
and private overlay. The complete base content/mode identity is unchanged.
The candidate installs the selected hardened nsinit, Wasip2 controller,
supervisor/gateways, worker SDK, input rule, native service policy and xdoom.
The existing Debian Firefox ESR engine, uBlock, FreeDoom WAD and desktop defaults
are preserved; Firefox now has the locked local SOCKS/DNS route. A custom x86
Firefox engine build is not claimed. Both compute menu entries remain pending
compute acceptance.

Claude completed a focused seven-turn packager review. Its SDK closure, privacy
cleanup and umask findings were resolved: every SDK leaf must be covered,
absolute/escaping SDK symlinks are refused, copied trees are compared, walk
errors fail, directory modes are stable, and common session/private state is
cleared. Machine IDs/hostname/hosts use a fixed public image identity rather
than the build container identity; per-session unlinkability is not claimed.
Six malformed SDK cases fail before output creation. Final SquashFS inspection
checks all fourteen component pins, parity assets and minimal input rules.
Build tools/host remain trusted; bit-reproducible timestamps are not claimed.

The exact candidate passes eleven real KVM control/CLI checks using the pinned
kernel. Root and real model disks are read fully through read-only dm-verity
mappings (564,912,128 and 2,546,024,448 data bytes). Four separate corruption
cases receive kernel EIO for root data/hash and model data/hash. Only disposable
qcow2 overlays are modified. The image's trusted model-volume descriptor pins
the model hash tree, and the native service checks model/policy/calibration
hashes. Production authentication of these pins remains a launcher obligation.

Inside the real confined desktop, UID1000 with zero capabilities, NoNewPrivs and
seccomp successfully runs clinfo -l, nvtop -s, llama-cli --version and sd-cli
--help. clinfo lists the Shield logical device; the Shield-aware nvtop displays
logical service counters and null physical GPU telemetry. A short native CPU
Qwen inference succeeds with native-model-v1 arithmetic. No GPU execution is
implied: the synthetic root-owned broker receives three readiness probes only.
The full owner-gated control graph, VNC, Sunshine server-info tunnel, restart,
clean stop and supervisor-death cleanup also pass.

This is a candidate image, not an admitted/published enclave app. The test uses a
fixed Wasmtime UID7501 adapter and synthetic owner credentials. Production M3
launcher/cgroups/exclusive-guest admission, authenticated worker reservation and
routing, platform attestation/authorization, GPU numerical/performance acceptance,
real Moonlight streaming, NucBox deployment and wallet publication are still open.
No end-to-end GPU speedup is established. The live controller, desktop, worker
and Moonlight client remain running unchanged. Candidate and evidence are in
native-root-candidate and evidence/native-root-image under outputs/kvm-box.


## Native resource primitives qualified — 2026-10-24

The new M3 desktop resource package now passes two real KVM tests on the pinned
kernel and selected immutable root/model volumes. It verifies canonical profile
and controller binding, actual active dm-verity tables, read-only filesystem
roots and mount identities. It retains resource descriptors through launch.
An atomic guest tmpfs claim admits one contender out of 32 and stays consumed
through errors and teardown. The single-use claim cannot create another cgroup
after removal. CPU quota, 8 GiB memory, no swap, group OOM, 1024 tasks and whole
descendant cleanup are enforced through the same cgroup fd used for clone.

Claude found claim-object reuse and bind-mounted-subdirectory acceptance; both
were fixed and tested inside KVM. The OOM test records an actual kernel group
OOM killing the allocator and an idle sibling. CPU/process-limit and descendant
cleanup checks pass separately. Go race tests and vet pass. Disk enumeration
now tolerates changing sd names and still authenticates data with verity hashes.

This package is not yet wired into production monitor/domexec admission, which
remains disabled. Worker lease/authentication and the final measured release
binding remain unresolved. No GPU work or owner secret is used by these probes.
Evidence, failures and review resolutions are in evidence/native-resources.


## Native broker endpoint prepared — 2026-10-24

Claude implemented an opt-in root-owned Unix endpoint in the shared broker.
The reviewed final code passes all eleven registered tests inside KVM, with
real credential-dropped clients. Root and same-group impostors are rejected
before upstream dialing; the authorized UID succeeds and authenticates a root
server peer. Existing broker behavior, connection limits and cancellation pass.
Host race tests/vet and both legacy broker command builds pass.

The native launcher must still authenticate/reserve the worker and bind its
lifetime to the desktop claim before publishing this endpoint. The option is
not enabled by any production caller yet. No GPU work occurs in these tests.
See evidence/native-broker-endpoint for sources, Claude's draft and KVM results.


## App-bound worker transport qualified in KVM — 2026-10-24

The new native worker lease binds one guest boot/claim to the host's verified
AppID, profile digest and GPU share. Only the root monitor holds its stream
capability. Control disconnect or unexpected bytes revoke every stream, with
no second claim or reconnect. Both sides bound pending/active streams to eight
and drain canceled dials. This is the NucBox operator-trusted T0-hv transport;
it does not make the GPU trusted or provide host-excluded SNP authentication.

Claude implemented the guest client draft. Review added pending-dial drainage,
closed-buffer handling and short-write checks. Host/guest race and socket tests
pass. Actual KVM qualification connects the guest Go lease and root-owned Unix
broker to the Node host server through vsock: a foreign VM is refused before
authentication, root and same-group Unix impostors are refused, 32 successive
UID 1001 clients each exchange 40960 public bytes, and revocation closes a 33rd
held stream. All 33 mock worker streams drain. No GPU operation is performed.

Investigation of the first failed integration run found a reproducible bug in
the existing vsock connection wait. The old blind-first-wait pattern times out
20/20 times on an already-connected socket. The fix checks actual connection
state before waiting; unconnected/refused cases remain rejected. The final KVM
run and existing front/monitor regression checks pass with this fix. Historical
failures and diagnostic passes are retained in evidence/native-worker-lease.

The Windows bridge itself, manager GPU allocation, separate image selection,
production monitor/native launch, release binding and actual GPU workloads are
not yet integrated by this phase. Desktop admission remains disabled. No owner
secret, running user service, NucBox deployment or publication was changed.


## Native NucBox standard CLI acceptance (2026-10-25)

Packaged sd-cli and llama-cli now pass inside the native desktop role through
the standard OpenCL frontend, with 392 and 511 verified masked exchanges. The
fixed sd PNG and llama answer match their deterministic references. A native
loopback policy bug blocked llama-cli's own helper; the fix preserves cross-role
and control-port denial in real nftables tests. See
`../shield-graphics/qualification/native-v7-cli-checkpoint.json` and `CTS.md` for
scope and evidence. This is a privileged lifecycle fixture, not real owner
provisioning or final production publication. The earlier statement that no
standard API exists describes the October 23 prototype only. Final menu cleanup,
GUI/Moonlight acceptance, branding and wallet deployment are still outstanding.

## Signed desktop v8: menu and CLI acceptance (2026-10-25)

`native-v8-desktop-checkpoint.json` records the new signed image, launch
measurement, root profile and fresh hardware checks. The two diagnostic compute
menu entries are removed. A real native desktop run verified all 14 seeded home
files, their ownership, modes and contents. The other 13 defaults preserve the
lightweight Firefox profile and desktop styling. Firefox, Terminal and DOOM menu
entries remain. The rebuilt root changes only the startup binary that embeds
these defaults; 26,031 other paths, including graphics and compute libraries,
are byte-identical to the preceding root.

On that unmodified root, packaged sd-cli generated the reference PNG with 392
verified masked exchanges (51,921,289,216 MACs); llama-cli completed the fixed
prompt with 511 exchanges (14,478,327,808 MACs). Both exited zero. The desktop
role had UID 1000, zero capabilities and seccomp, without physical GPU device
nodes or VSOCK access. This lifecycle fixture adds a test initializer/monitor
and longer front wait; it does not establish real owner provisioning.

Separately, the exact signed v8 production image loaded its genuine Wasm
controller and passed two fresh app-attestation challenges, 26 guest negative
checks, and a fresh host TPM challenge with two negative checks. The controller
remained sealed. The diagnostic host SSH process needed forced closure after
its short cleanup grace; subsequent process inspection showed no new TPM helper
left behind. Both disposable VMs were removed and their VMGS files archived.
Production VMs and host boot/security state were unchanged during both tests.

NucBox is the documented host-trusted Hyper-V deployment; this does not exclude
the host OS. The catalog/deployment identity remains a fixture. Real owner
release, final GUI/FreeDoom/Firefox/Moonlight acceptance, production integration
and wallet publication remain required. General OpenGL/Vulkan rendering remains
CPU Mesa with only the supported masked integer-affine prepass; this is not
full GPU rendering, universal CUDA, game qualification, or Khronos conformance.

Evidence exports: `native-menu-build-v8`, `native-menu-cli-v8`, and
`native-app-attestation-v8` under the task's `outputs/kvm-box/evidence/`.

## Signed desktop v9: enforced launcher CPU allocation (2026-10-25)

`native-v9-cpu-checkpoint.json` pins the new image and its separate runtime and
hardware evidence. Only the monitor binary changed in the platform archive;
the v8 root, profile, media, default home files and graphics/compute libraries
remain byte-identical. The monitor now accepts a bounded launcher CPU quota
without changing the immutable catalog bundle or its application identity.

An eight-vCPU NucBox fixture loaded the four-CPU catalog bundle with an explicit
six-CPU allocation. The effective requirement was six processors and the actual
desktop cgroup read back `cpu.max=600000 100000`. Eight CPU consumers inside the
desktop raised `nr_throttled` from 0 to 59. The packaged sd-cli and llama-cli
then passed their standard masked-workload checks: 392 and 511 verified
exchanges, the reference PNG and the expected Paris answer. All 14 home defaults
passed again, and the removed diagnostic menu entries remained absent. This
proves guest enforcement, not purchase authorization or host pool accounting.

The exact signed production image separately passed two fresh app challenges
and 26 guest negatives, bound to a fresh host TPM challenge with two negatives.
The fixed Wasm controller remained sealed. The diagnostic TPM helper exited
normally within the longer cleanup grace and left no new helper process.
Both disposable test VMs were removed and their guest state archived.

The runtime test's production VM inventory was **not** unchanged: one app was
automatically replaced after RPC renewal failures and lease expiry. The first
failure preceded the test VM launch. The node reason and Hyper-V events are
included in the evidence; the RPC root cause remains unknown. The other three
production VMs, physical GPU worker and host boot/security stayed unchanged.
The separate attestation test had an unchanged production inventory.

The node compatibility package is prepared but unactivated. Manager integration,
real owner release, final GUI/FreeDoom/Firefox/Moonlight acceptance and wallet
publication remain required. General rendering still uses CPU Mesa with the
supported masked prepass. No universal CUDA, full GPU rendering, game support,
Khronos conformance or host-OS exclusion is established by these tests.

### Native v9 manager hardware checkpoint

The installed-host compatibility overlay now has a disposable NucBox launch
through its real Manager/router/WMI path. A purchased 600-percent allowance
reached the signed monitor with six online processors and the expected 12/18-GiB
application/partition memory split. Readiness correctly stayed closed before
owner release; fresh hardware evidence passed. The real masked worker channel
was authenticated, but this pre-owner run opened no GPU data streams. Cleanup
removed all test resources without changing the four production VMs. See
`../shield-graphics/qualification/native-v9-manager-checkpoint.json` and the
corresponding CTS section. Production activation and final provisioned compute,
graphics and Moonlight acceptance remain incomplete.
