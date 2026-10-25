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
Zink uses lazy descriptors and one llvmpipe worker, matching the desktop's
lightweight configuration. Vulkan loads the pinned Lavapipe ICD. No physical
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
synchronization coverage. Vulkan's separate synchronization groups remain pending.

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
