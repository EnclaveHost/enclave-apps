# Vulkan compute-to-draw qualification

This standard Vulkan application submits an integer compute shader, a memory
barrier, and a draw using the computed buffer as integer vertex attributes.
It reads back the rendered frame. It has no Shield service calls and never
opens a worker connection. The application requires a CPU Vulkan device so a
raw physical GPU cannot silently satisfy this reference test.

Eight dispatches check 2,048 scalar outputs against an independent integer
reference, including low-byte clamp extrema and nonzero high input bits.
Input contents and output guards must remain unchanged outside the specified
writes. The final 128×128 RGBA frame has 1,620 colored pixels.

Build in the Mesa tools image with `OUTPUT=/output/build sh /source/build.sh`.
The output directory must be new. Run `prepass SHADER_DIRECTORY NEW_RGBA_FILE`
with `VK_DRIVER_FILES` pinned to the Lavapipe ICD. The tool uses ordinary Vulkan
loader calls, SSBO descriptors, a compute pipeline, a graphics pipeline and a
fence; it does not select or implement a masked route.

The Debian Mesa 25.0.7 reference, an upstream Mesa 25.0.7 source build, and the
native KVM guest produced identical final RGBA bytes. This establishes a
reference for later protected lowering. Those initial reference runs demonstrate no GPU offload or speedup. The
[patched ICD](../driver/README.md) subsequently offloaded the prepass through
Shield with identical outputs. This is not a graphics API conformance suite.

The public synthetic transform is `(x+y, y-x, z, w)` after each source word is
masked to its low byte and shifted by −128. The structural compiler proves those
semantics, and each dispatch revalidates the runtime buffer layout before replacement. Approval of this
published test matrix does not authorize private scene coefficients, camera
matrices or other shaders. Equivalent compiled modules may qualify; unknown semantics stay local.


## Regression fixtures

Run `python3 build-variants.py --output /output/variants` inside the pinned
Mesa tools image. The generated ordinary Vulkan applications test base and
device-generated dispatch, specialization, dynamic descriptors, wider ranges,
and pipeline-statistics queries. Failure variants use infinite fence/timeline
waits, a waiter started before submission, and queue/device idle waits.

`run-driver.py` runs each case in a disposable container with no physical GPU
nodes. Supply immutable `--image`, `--mesa` (installed prefix), `--sdk`, and
`--output` paths. The output directory must contain `build/`, `variants/`, and
`triangle.rgba` from the unmodified reference ICD. Each case creates new logs
and refuses to overwrite evidence. Only the masked, equivalent-module and worker-corruption cases enable
host networking, solely to proxy to the dedicated loopback `--worker-port`;
the Vulkan app receives only the guest-local service socket. For example:

```sh
python3 run-driver.py --image sha256:TOOLS_HASH --mesa /work/mesa-install \
  --sdk /work/native-sdk --output /work/qualification masked
```

Use `build-admission-fixtures.py --output /output` in the tools image to create
`build-equivalent/` (renamed but equivalent shader) and `build-unmatched/`
(explicit bound-check control flow outside the supported proof). The equivalent
case uses the masked route; unmatched remains local. Both use validated SPIR-V.
The service and GPU policy are synthetic fixtures; successful tests do not
establish production admission or attestation.


## OpenGL compute reference

`prepass-gl.cpp` and `prepass-gl.comp` exercise ordinary OpenGL compute through
Zink on the pinned Vulkan ICD. Build with `OUTPUT=/output/gl sh /source/build-gl.sh`
in the tools image, then run `prepass-gl prepass-gl.comp` on the desktop display.
It checks 2,048 outputs across eight dispatches, SSBO alignment, untouched guards
and unchanged input using independent integer arithmetic. It requires a Zink
llvmpipe renderer and claims no GPU offload.

The structural driver also proves Zink's generated set-3 descriptor array and
four scalar stores. `gl-masked` checks actual worker/service counters; the app
itself makes no offload claim because standard OpenGL does not expose them.
Copy `prepass-gl` and `prepass-gl.comp` into the driver output directory. Run
`gl-local`, `gl-masked`, `gl-unavailable`, `gl-queries`, `gl-wide-range`,
`gl-descriptor-buffer`, and `gl-worker-corrupt` with `run-driver.py`.

`--expect-device-lost` creates a robust context, checks reset notification and
`GL_CONTEXT_LOST` on a subsequent command, and skips readback. `--queries` checks
64 compute invocations per dispatch; `--wide-range` exercises local fallback.
The harness selects the private Mesa libraries and lazy Zink descriptors;
the descriptor-buffer test overrides that selection and must stay local.

`test-nir.py` is the offline compiler admission suite. Prepare the source/tool
as described in the driver README. It compiles and validates every SPIR-V input,
then checks acceptance, named contract and rejection reason. These tests are
independent of the real-ICD/GPU execution checks and do not replace them.

## Desktop browser workload

`browser-webgl.html` exercises WebGL 1 and 2 in the installed Firefox browser.
Each API checks every RGBA channel of a 128×128 reference framebuffer with
nearest-neighbor textures, overlapping depth-tested geometry and alpha blending
(tolerance one 8-bit channel value). It then renders a 512-triangle animation on a 640×360 canvas,
records 80 frames after warmup, and reports frame intervals and render-call times.
The latter include JavaScript geometry updates, draw submission and `gl.finish()`;
they are not hardware GPU timer measurements. The lightweight profile targets
30 FPS. These results characterize this workload, not game compatibility.

`probe-browser.py` runs as the actual native desktop user on Xorg, retains the
installed profile, and obtains the result from the controlled page's X window
title. No HTTP or automation listener or new firewall exception is needed.
Place the HTML/probe and `capture-reference.py` under `/tmp`, with the existing
`/tmp/graphics-evidence` directory, then invoke the probe with the measured
desktop graphics environment. It records the profile hash, loaded private Mesa
libraries and a screenshot, and terminates only the browser process group it
started. The native harness separately checks that worker counters do not
change during this CPU-rendered workload.

The native browser checkpoint uses the existing x86 Firefox 153.5.1 and the
unchanged lightweight profile (`61137066…5379`). Both APIs passed 65,536 channel
comparisons each, at about 30.00 FPS. Median render-call time was 2 ms; p95 was
7 ms for WebGL 1 and 3 ms for WebGL 2. Both p95 frame intervals were about 34.1 ms.
This is a short controlled workload in a 2-vCPU/8-GiB KVM guest, not a game soak.
The process mapped both pinned private Mesa libraries, retained one llvmpipe
worker thread, and made no compute-worker exchanges. The native driver prepass,
corruption/retry and lifecycle checks passed in the same run.

Firefox truncates long native window titles. The probe therefore collects
bounded numbered chunks with a per-report ID, rejects inconsistent chunks,
and decodes only a complete report. The page itself displays the full result.
