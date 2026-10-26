# Selected Khronos graphics compatibility tests

The existing custom compute-to-draw fixtures establish the masked operation's
behavior. Ordinary applications also need buffer storage, synchronization,
texture barriers, resource binding and graphics pipeline behavior. This runner
uses unmodified public Khronos tests to check those areas against the exact
private Mesa libraries in the native desktop candidate.

This is a selected compatibility test, not a Khronos conformance submission or
certification. The container has no Shield socket, physical GPU nodes or network.
These runs exercise trusted CPU fallback only. They do not prove masked offload,
native VM isolation, attestation, game compatibility, streaming performance or
production deployment. The native masked/corruption/lifecycle tests remain
separate acceptance requirements. All of these requirements must pass against
the final candidate before publication.

## Pinned inputs

`cts-lock.json` records the official `vulkan-cts-1.4.1.0` source commit
`c2dcff23a48c7cb17efc51ee634b027aa6770889`, every dependency revision selected
by that commit's upstream fetch script, downloaded archive hashes and lengths,
the tools image, and both qualified Mesa library hashes. The public CTS lacks
the confidential GTF tests. System zlib/libpng are used from the tools image;
optional RenderDoc integration is disabled. No test source is patched.
Upstream CMake replaces two jsoncpp dependency files with its own bundled
`config.h.cts` and `json_valueiterator.inl.cts`; those expected configuration
copies are part of the pinned upstream build, not local test changes.

The tools image is a separate build artifact; it does not modify the host or
desktop image. It was built from the Mesa validation tools base
`sha256:9c97d769ffa3e605b415ad7165f687994d6178aabcce836943b45ecaa17fd825`
with `cmake git libpng-dev libx11-dev libxext-dev libxrender-dev libxrandr-dev
libegl-dev libvulkan-dev` added using Debian packages. The final immutable image
ID is pinned in the lock. Re-running package installation later is not guaranteed
to reproduce that image; preserve its package inventory and image with evidence.

## Build and run

Use Python with safe tar extraction support (`filter='data'`) and Docker. Replace
the example absolute paths with fresh working directories and the candidate's
installed private Mesa prefix. The runner expects UID 1000 to be able to write
the output directory. It never overwrites a prior run.

```sh
python3 fetch-cts.py --output /work/cts-new
python3 build-cts.py --cts /work/cts-new
python3 run-cts.py --cts /work/cts-new --mesa /work/native-sdk/mesa \
  --output /work/cts-gl-run --api gl
python3 run-cts.py --cts /work/cts-new --mesa /work/native-sdk/mesa \
  --output /work/cts-vk-run --api vk
```

The build is offline after download and is limited to two CPUs and 4 GiB of
memory without swap. OpenGL uses two compiler jobs; Vulkan uses one because
its shader precision translation unit exceeded the initial 3 GiB/two-job limit.
It builds `glcts` followed by
`deqp-vk`, so a completed OpenGL binary can be qualified while Vulkan builds.
Both use the X11 EGL/GLX platform. The test container has an immutable root,
read-only source/build/Mesa mounts, no capabilities, no new privileges, and only
its new evidence directory and temporary directories writable.

`cts-plan.json` selects six groups per API. OpenGL covers buffer storage, shader
storage, compute, synchronization, texture barriers and direct state access.
Vulkan covers both synchronization APIs, depth, input assembly, dynamic offsets
and pipeline caching. `--group NAME` can select one or more listed groups for
diagnosis; retain the exact selection in the report. It is not equivalent to
running the full plan.

The OpenGL runner starts and terminates its own container-local Xvfb server.
It selects EGL by default; `--gl-context glx` tests GLX context creation
explicitly. Preserve context creation failures as failed qualification evidence.
The container uses Zink's lazy descriptors and one llvmpipe worker. The final
native image sets two llvmpipe workers; native qualification must retain that
shipped setting and report the difference. Vulkan loads the pinned Lavapipe ICD. No physical
GPU device or Shield service may be present. The driver cannot obtain a raw
hardware path from this test environment.

## Evidence and interpretation

For each group, the runner first enumerates the selected test names, then runs
exactly that pattern. It saves stdout, the full QPA result, per-case status and
diagnostic text, elapsed time, exit code and observed process library mappings.
The pinned upstream QPA parser reads results. The runner checks that all
enumerated cases appear exactly once, no extra cases appear, no record remains
unfinished, execution finishes without a timeout or process error, and the test
process actually maps the required private Mesa libraries. Library contents are
hashed before execution; the test binary hash is also recorded.

`allSelectedPassed` requires every selected case to report `Pass`. Unsupported
cases, warnings and errors are never counted as passes. A separate
`noFailuresWithUnsupported` field can describe complete runs containing
`NotSupported`; it is not the acceptance verdict. Missing/duplicate cases,
truncated logs, unexpected libraries, timeouts and nonzero exits fail the check.
Each group has a 15-minute execution bound; a timeout requires investigation
and a new evidence directory, never deletion of its partial log.

`test-cts-results.py` tests these evidence failure modes. Its passing result
validates bookkeeping only. Actual CTS results and native application workloads
are needed to assess the driver.

## OpenGL checkpoint, 2026-10-25

The six selected groups completed 551 cases through EGL/Zink using the exact
candidate libraries: 550 `Pass`, one `CompatibilityWarning`, no unsupported
cases, missing results or duplicate results. Counts were buffer storage 9,
shader storage 124, compute 42, sync 1, texture barrier 4, and direct state
access 371. The sync group contains only `flush_commands`; it is not exhaustive
synchronization coverage. Vulkan's separate synchronization results are recorded below.

`KHR-GL45.direct_state_access.framebuffers_check_status` reports
`GL_FRAMEBUFFER_COMPLETE` where two optional subcases expect incomplete
multisample/layer-target statuses. The pinned test source explicitly classifies
these optional checks as compatibility warnings. All 371 direct-state-access
case statuses and details match the separately pinned, unmodified Mesa 25.0.7
reference. The warning remains in the result and `allSelectedPassed` is false;
it has not been suppressed or converted to a pass.

GLX context creation fails before enumeration because the tested Xvfb/GLX path
does not advertise `GLX_ARB_create_context_robustness`, which this suite's
context request requires. This failure also reproduces on the unmodified
reference. The successful EGL run does not qualify the GLX path. Existing native
GLX fixture results are narrower and do not replace this missing qualification.

Evidence is saved separately as `graphics-cts-opengl-v1`, including all per-case
results, logs, exact selections, binary/library identities, baseline comparison,
source audit and runner code. General rendering remains CPU-based. This
checkpoint is neither native VM qualification nor complete API conformance.

## Native OpenGL checkpoint, 2026-10-25

The same 551 selected cases subsequently ran inside the actual native desktop
on its existing Xorg `:0` display: 550 passes and the same one compatibility
warning. `allSelectedPassed` remains false. Every group mapped both pinned
Mesa libraries. The desktop kept its shipped `LP_NUM_THREADS=2` and lazy Zink
descriptors. The UID-checked Shield service's counters were unchanged across
the complete run, so this is CPU fallback evidence, not masked GPU execution.

The test used the unchanged signed v4 candidate platform archive, profile,
root filesystem and model media in a disposable local KVM VM. Explicit fixture
entry points were appended to a separate initrd. The actual monitor/domain
implementation loaded the expected controller/profile/runtime, kept the
controller's secret pipe sealed, started the desktop, and cleaned up the domain
and cgroup. Its one-use admission checks also passed. The fixture's privileged
controller client is not owner-release or hardware-attestation evidence.

`cts-native-role.c` opens a detached copy of the read-only test-assets mount
before joining the desktop namespaces, transfers it with `move_mount`, closes
inherited descriptors, and applies the real desktop role policy. It never ships
in the production image. `cts-native.py` requires UID 1000, zero capability sets,
no-new-privileges, seccomp, the expected guest kernel, and no physical GPU nodes.
It runs the unchanged CTS binary, records full case logs and library mappings,
and compares results against the container reference. Only the explicitly
reviewed framebuffer compatibility warning may remain; a failed reference
cannot authorize a later failure. The warning still never becomes a `Pass`.

The runtime image has versioned GL/EGL/GLES loader libraries, whereas this CTS
build calls `dlopen` with unversioned development names. The fixture creates
three aliases in its own temporary directory pointing to the existing,
hash-checked runtime libraries. It records the aliases and prepends only that
directory to the test command's library search path. No driver or production
filesystem bytes are replaced. API assertions and case selection are unchanged.

Evidence `graphics-cts-native-opengl-v1` contains 79 files plus a hash manifest:
full native QPA logs, lifecycle result, profile/media hashes, fixture source,
build/run recipes and the three unsuccessful setup attempts. A captured host
scope sample confirms the 4.5 GiB/no-swap/two-CPU limit; a host memory guard
protects a 16 GiB available-memory floor. The test VM exited, its native domain
and cgroup were removed, and the separate Vulkan compiler resumed.

This qualifies the selected EGL/OpenGL workload in the native fixture. GLX CTS
context creation, broader Vulkan coverage, real application workloads, owner
provisioning and deployment remain separate work. It does not claim full API
conformance, general protected GPU acceleration, or gaming performance.

## Vulkan container checkpoint, 2026-10-25

The six selected groups completed all 9,300 cases with 4,371 `Pass` and 4,929
`NotSupported`, no failures, missing or duplicate records, and the pinned
Lavapipe library observed in every test process. `allSelectedPassed` is false.
The exact case names, statuses and diagnostic details match an independent run
against the separately pinned unmodified Mesa 25.0.7 libraries.

| Group | Pass | NotSupported |
| --- | ---: | ---: |
| Synchronization | 21 | 3 |
| Synchronization2 | 14 | 3 |
| Depth | 3,199 | 4,923 |
| Input assembly | 148 | 0 |
| Dynamic descriptor offsets | 216 | 0 |
| Pipeline cache | 773 | 0 |

The unsupported cases comprise 4,015 depth-bounds tests, 908 tests of
`VK_FORMAT_D16_UNORM_S8_UINT`, and six semaphore tests requiring multiple queues.
The pinned upstream synchronization test explicitly requires a second queue;
Lavapipe exposes one queue. The depth tests check the advertised feature and
format support before running. Unsupported results remain visible; this evidence
does not establish support for those features or full Vulkan conformance.

The unmodified `deqp-vk` binary SHA-256 is
`2bb9cb3260fa3cb35c9c67845f098115a89a9522391b4a3c4063ef261805fb82`.
Native fixtures select `vk` via a read-only `native-api` asset, use that exact
binary and its upstream runtime data, and retain the shipped desktop settings.
`matches_optional_reference` requires a complete case inventory and explicit
case-name/reason pairs for reviewed unsupported results. New unsupported cases,
changed reasons, failures, warnings, missing cases and duplicates are rejected.
An optional feature improving to `Pass` is allowed. This comparison never changes
the raw statuses or the all-pass verdict. These CPU compatibility tests require
unchanged Shield worker telemetry; masked execution is qualified separately.

## Native Vulkan checkpoint, 2026-10-25

All 9,300 selected cases also completed in the actual native desktop role with
the same 4,371 passes and 4,929 unsupported results. Each case's status and
unsupported diagnostic matched the reviewed reference. Every group mapped the
pinned Lavapipe library. The fixture retained desktop UID 1000, zero
capabilities, no-new-privileges, seccomp, no physical GPU devices and two
llvmpipe workers. The UID-checked Shield service's telemetry was unchanged.
No GL development-library aliases were needed for the Vulkan binary.

The exact signed v4 candidate root/profile/model media and production platform
archive remained unchanged. A separate test initrd
`a89470f68d5d60190986634e555ce5542f9fbc6cc67d7033629a11203f408fd6`
appended 1,596 test files. The real monitor/domain lifecycle passed, including
sealed controller secret, one-use claim, and domain/cgroup removal. The test VM
and host scope exited. Its privileged fixture client is not owner provisioning,
NucBox deployment or hardware attestation.

The fixture permits 900 seconds per Vulkan group; a test-only Go overlay raises
the graphics-command deadline to 1,500 seconds, with explicit guest/host
lifecycle deadlines of 1,560/1,620 seconds. Production timeouts are unchanged.
The actual complete host run took about 76 seconds under a 4.5 GiB/no-swap,
two-CPU scope and 16 GiB available-memory guard. Timings across container and
native runs are not a controlled performance comparison: desktop worker/cache
settings differ. No gaming or GPU speedup is inferred.

Evidence `graphics-cts-vulkan-v1` retains both container runs and their exact
comparison. `graphics-cts-native-vulkan-v1` retains full native logs, pinned
inputs, fixture source and lifecycle evidence. `allSelectedPassed` remains
false in all three runs. This is selected Vulkan compatibility evidence, not
conformance certification or general protected hardware rendering.

## GLX robustness discovery fix under qualification

The native v4 GLX attempt also stopped before enumeration because neither the
server nor combined GLX extension string advertised robustness. A separate
probe established that the client supports it and can create a direct context
whose queried robust-access flag and lose-context-on-reset strategy are set.
The issue is discovery, not evidence that reset behavior can be ignored.

The private Mesa patch marks robustness as available through a capable direct
renderer independently of the server's indirect extension list. Mesa still
requires the actual driver reset-query capability before enabling direct
support; no extension override or CTS option suppresses the check. When no direct renderer is available, the existing server-dependent
negotiation path is unchanged. See the
[Khronos extension specification](https://registry.khronos.org/OpenGL/extensions/ARB/GLX_ARB_create_context_robustness.txt)
for context flags, reset strategies and sharing requirements.

With the rebuilt GLX library, all 551 original GLX CTS cases ran in the isolated
container: 550 passes and the same framebuffer compatibility warning as EGL.
The exact GLX library hash and process mapping were checked. The regression
probe `glx-robustness.cpp` checks normal extension discovery, the direct context,
actual robust flag and reset strategy, and rejection of incompatible sharing
and invalid reset attributes. Its discovery assertion rejects the old library.

The changed library subsequently completed the same 551 cases in the native
VM: 550 passes and one unchanged warning. All groups mapped its exact hash;
Shield telemetry was unchanged, and the domain/cgroup/one-use lifecycle passed.
The test helper mounted only this library read-only over its original path in
the disposable desktop namespace before dropping privileges. This explicit
overlay is a qualification fixture, not a new signed production image.

This fix is not yet in the signed v4 image. Masked failure regression checks
and a newly built/qualified image remain required before release. Both the
failed original native run and successful overlay run are retained alongside
the container evidence in `graphics-glx-robustness-v1`.

## Masked GLX regression, 2026-10-25

The corrected GLX library also passed the actual native masked-work fixture:
16 verified affine exchanges, 131,072 MACs and 4,096 checked shader components.
Corrupting GPU reply 17 caused OpenGL context loss; subsequent OpenGL and Vulkan
attempts failed against the retired service without an eighteenth GPU stream,
CPU retry, readback or later draw. The host fixture verified the pinned dedicated
RTX 3070 worker and left it running. Domain/cgroup cleanup and one-use admission
passed. Evidence is `native-glx-masked-v1`.

The first attempt stopped before GPU work because two Vulkan frames were
identical. It is retained. The fixture now samples for up to eight seconds while
requiring the same more-than-1,000 changed pixels; the successful run needed only
one sample for each program. The original sample's cause is unestablished. This
is an explicit single-library overlay test, not final-image owner provisioning.

The CTS lock now pins the selected GL loader in addition to the core libraries:
EGL must map the pinned EGL loader; GLX must map the corrected GLX loader. Vulkan
has no GL-loader requirement. Both selected-loader smoke cases pass, and the
old GLX library is refused by its hash before test execution. Historical
fixtures retain their exact older lock and runner; do not rewrite their evidence.


## Embedded driver in signed v5, 2026-10-25

The GLX fix is now embedded in the immutable v5 root rather than supplied by
an overlay. The signed IGVM SHA-256 is
`48d3eb8c97983fc4affebf7e8606c24916fb338c9f2bef86753ee735704843cc`;
the profile is
`e000a9577f0686a104494931ec66e41af22d2a0c552232d5fa965721d33a380e`.
An independent Python cryptography verifier checked its P384/SHA384 signature
against the reproducible IGVM builder's launch digest and refused an altered
measurement and wrong hash algorithm.

Separate disposable local KVM runs used the exact v5 root/profile/model media
and production platform archive. Only test programs and an explicit initializer
were appended; no driver library was overlaid. GLX and EGL each completed all
551 selected cases (550 Pass, one unchanged CompatibilityWarning). Vulkan
completed all 9,300 cases (4,371 Pass, 4,929 reviewed NotSupported). The required
loader/core-library hashes and process mappings were checked, and Shield
telemetry stayed unchanged. `allSelectedPassed` remains false for all suites.

The GLX run additionally limited model storage reads to 32 MiB/s. The desktop
reported running after 78,794 ms, within the 115-second inner startup budget.
This is one controlled slow-storage check, not general startup reliability.
The separate masked test verified 4,096 shader outputs and 131,072 MACs in 16
GPU exchanges. A deliberately corrupted seventeenth reply retired the service;
subsequent GL/Vulkan attempts failed without another GPU connection, CPU retry,
readback or later draw. Both graphics windows produced changing pixels, and
the GLX robustness discovery/context checks passed from the embedded library.

All runs retained desktop UID 1000, zero capabilities, seccomp, no physical GPU
nodes and sealed controller secrets. Native domains/cgroups and temporary VMs
were removed. General rendering remains trusted CPU Mesa. These fixtures do
not qualify owner release, fresh v5 hardware attestation, final CLI/browser/
Moonlight workloads or production publication. The earlier v4 hardware evidence
cannot establish the changed v5 measurement.

`native-v5-checkpoint.json` records exact scope, counts and identities. The
exported `native-graphics-final-v5` evidence directory contains full CTS case
logs, fixture source, lifecycle results, masked failure evidence and a verified
hash manifest. Its separate `native-platform-v5-signed-graphics` package retains
historical export-time pending flags; later qualification is recorded separately.


A separate NucBox run then booted the exact signed v5 IGVM and loaded the fixed
Wasm controller. Two fresh verifier challenges passed the hardware-bound app
verifier for v5's new launch measurement, public signing identity, SVN 13,
profile-derived app bundle, runtime and independently observed TLS key. All
26 guest negative checks and two host negative checks were rejected. The
prepared additive relay policy accepts these captured reports at SVN floor 13
and rejects floor 14 without changing legacy image selections. That policy
remains unactivated. See `native-v5-hardware-checkpoint.json` and exported
`native-app-attestation-v5` evidence, including a standalone replay verifier.

The controller remained sealed; this does not establish owner release or a
running desktop on NucBox. The test VM was removed and its guest state archived.
Original VMs, host boot, Secure Boot and VBS state were unchanged.

The subsequent standard CLI check in a local native v5 fixture remains
incomplete: the 4.5 GiB host test scope was OOM-killed before either application
returned a completed result. A prepared 6 GiB retry has not launched because
its preflight preserves 16 GiB of available memory for concurrent work.
`native-cli-v5-incomplete` retains the failure evidence; it is neither a CLI
pass nor evidence of an application defect. No production resource policy was
changed in response. Complete this acceptance with sufficient test capacity
before publication or removing the diagnostic menu entries.


## Worker startup failure and v6 candidate, 2026-10-25

A NucBox CLI fixture exposed a panic when a failed vsock dial returned a typed
nil connection through a non-nil interface. The monitor cleanup then called
Close on that nil pointer. The production adapter now returns a nil interface
with the error. A canceled-context regression against the real adapter reproduces
the old panic and passes after the fix; the worker race suite and native monitor,
desktop and worker tests pass. Platform source commit: `3655ccf8d`.

The signed v6 image SHA-256 is
`8a9b640bb468c6109f9a2bf9ca741da29df8be2b778d1694ed6af6768ca8e750`.
Independent archive inspection shows only the monitor changed from v5. The
immutable root, graphics libraries, runtime, controller, profile and media are
identical. Repeated IGVM builds match, all five measurement mutations move the
launch digest, and independent public-key verification accepts the signature
and rejects an altered measurement and the wrong hash algorithm.

`native-v6-build-checkpoint.json` and exported `native-worker-dial-fix-v6`
evidence cover the regression and build. They do not establish v6 hardware
attestation, CLI acceptance or owner release. The signed image is separate and
unadmitted; historical v5 hardware reports cannot attest it. The failed CLI
fixtures are retained, and the next test explicitly waits for the host bridge
before loading the controller. That fixture-only delay is not a production
startup-policy change or performance result.


The exact signed v6 image subsequently passed fresh NucBox hardware-bound app
verification: two independent challenges, 26 refused guest negative cases and
two refused host negative cases. The actual controller remained sealed. The
prepared additive relay policy accepts the captured reports at SVN floor 13 and
rejects floor 14 with the hardware signature still valid; it is not activated.
`native-v6-hardware-checkpoint.json` and exported `native-app-attestation-v6`
record this separate scope. Its bundled verifier replays independently. The
attestation VM and transports were removed; the original four VMs, host boot,
Secure Boot and VBS status were unchanged.

The next v6 standard CLI fixture reached native desktop startup and opened 115
authenticated worker streams, but did not return an application result. The
front's five-minute upstream-listener deadline ended the deliberately sealed
controller's domain. Exported `native-cli-v6-front-timeout` retains this failed
run and cleanup evidence. Stream admission counts are not verified compute
results. A follow-up disposable fixture extends only that front wait budget and
test command deadlines; it must disclose this difference and cannot qualify
production owner provisioning. Production front behavior remains unchanged.


### Standard native CLI acceptance and v7 loopback policy (2026-10-25)

The corrected extended-front fixture completed sd-cli with 392 verified masked
exchanges and the expected PNG. llama-cli loaded its model but remained waiting
for its own HTTP helper until the 900-second test deadline. The native nft policy
blocked UID 1000 loopback TCP; this was not a GPU transport stall. Exported
`native-cli-v6-sd-pass-llama-loopback` preserves the failed suite and sd success.

The policy now marks desktop-initiated 127.0.0.1 TCP and requires UID 1000 on
replies before established-flow acceptance. All fixed control and private-service
guards remain. A real nftables regression reproduces the old failure and passes
19 expectations with the change, including denial of cross-role helpers. The
monitor embeds exactly the generator's policy. Native Go suites pass.

A fresh NucBox fixture with that policy passed both packaged commands: sd-cli
392 verified exchanges / 51,921,289,216 MACs and the exact reference PNG;
llama-cli 511 exchanges / 14,478,327,808 MACs and the expected Paris answer.
The CLI processes retain UID 1000, zero capabilities, seccomp, VSOCK denial and
no physical GPU nodes. The test VM was removed, its VMGS archived and transport
closed; production VMs and host security remained unchanged during this run.
`native-v7-cli-checkpoint.json` and exported `native-cli-loopback-pass` preserve
this scope. These small deterministic cases do not prove general numerical
quality, speed, arbitrary CUDA compatibility or complete graphics conformance.

The associated separate signed v7 candidate is
`abbbecfeef734933cd50624dfe5f591823c592da7e3bef21239a27688d04110d`;
launch measurement
`d82dcdbb9f37abedbe408d89509e02dabe7c255734d92345fd7997a5bdb99d4e`.
Only its monitor changed from v6. Independent signature and archive checks pass.
The CLI fixture used a test monitor/front over the immutable production root;
it is not fresh hardware attestation of the signed candidate or owner release.
Final GUI/Moonlight acceptance, requested menu cleanup, wallet publication and
production deployment remain outstanding. General rendering remains CPU Mesa.

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

## Signed desktop v9: manager hardware path (2026-10-25)

`native-v9-manager-checkpoint.json` records a launch through the installed-host
compatibility Manager, router and WMI launcher. The unchanged four-CPU catalog
bundle received a purchased 600-percent CPU allowance, six online processors,
12 GiB application RAM and an 18 GiB partition. Both host processor settings and
the monitor quota read back exactly. The 109-module runtime was assembled from
pinned installed sources and the committed compatibility overlay; its preceding
manager suite passed 281 tests, plus eight packaging checks.

The actual readiness judge verified the document on the app's TLS connection,
but correctly refused readiness while owner secrets were missing (HTTP 503).
Two separate fresh app challenges and 26 negatives passed the hardware verifier,
with a fresh host TPM challenge and two negatives. The real masked-worker and
egress transports started. No GPU data stream or owner release occurred. The
component came from a local hash-pinned fixture fetcher, so this does not qualify
live IPFS retrieval, catalog registration or purchase authorization.

The test VM was removed, its guest state archived, every test process closed,
and CPU/GPU/physical RAM reservations returned to zero. All four production VMs,
the GPU worker and host boot/security state remained unchanged during this run.
The evidence is `outputs/kvm-box/evidence/native-manager-hardware-v9/`; its bundled
verifier replays the captured evidence independently. Production activation, owner
provisioning, final desktop/Moonlight acceptance and wallet publication remain.
This is launch and attestation evidence, not a new graphics or compute benchmark;
the CPU Mesa and bounded masked-prepass limits above still apply.

## Signed desktop v9: live host pools and permanent assets (2026-10-25)

`native-v9-livepool-checkpoint.json` closes the launch-sizing difference between
the previous fixture and the live host. The separate test manager read the actual
16-CPU / 96-GiB application pools and applied a 25% purchase: four processors,
400% guest CPU quota, 24-GiB application allowance and a 36-GiB partition. Host
and monitor readbacks matched. Fresh app/host verification passed again with
26 guest and two host negative checks. The real readiness judge still refused
service before owner provisioning; no desktop workload or GPU data stream ran.
Cleanup removed the exact VM and all reservations. The four production VMs,
physical worker and host boot/security matched the before/after inventories.

The signed image, media, bridge, component and public release/configuration now
have a separate permanent, protected NucBox directory. All nine files were
independently rehashed there, and configuration identity and worker compatibility
checks passed. Existing service configuration and processes were unchanged.
The public controller gateway still returned 404, so normal wallet-authorized
upload and actual catalog/component retrieval remain open. Evidence is in
`native-manager-livepool-v9` and `native-durable-release-v9` under the exported
`outputs/kvm-box/evidence/` directory. These checkpoints do not establish owner
provisioning, production activation, final graphics/compute/Moonlight acceptance
or publication; the rendering and threat-model limits above remain unchanged.
